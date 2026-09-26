use std::{
    fmt,
    ops::ControlFlow,
    sync::Arc,
    time::{Duration, Instant},
};

use dekopon_config::Skill;
use dekopon_model::error::InferenceError;
use dekopon_model::model::{
    ChatModel, CompletionOptions, ContentPart, ModelMessage, ModelTool, ModelToolCall, ModelUsage,
    assistant_message,
};
use dekopon_model::{ModelText, TurnEvent};
use dekopon_shell::ScriptOutcome;
use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    improvement::{self, ImprovementSuggestion},
    meta::AgentConfigView,
    milliseconds,
    progress::{
        CancelSource, FailureClass, ProgressEvent, ProgressSink, STREAMED_TEXT_BOUND_BYTES,
        SessionOutcome,
    },
    skills::{self, SkillReads},
    wake::{self, WakeRegistrar},
};

mod history;

pub use crate::{
    improvement::IMPROVEMENT_TOOL_NAME, skills::SKILL_TOOL_NAME, wake::WAKE_TOOL_NAME,
};
pub use history::{ConversationTurn, DEFAULT_MAX_BYTES, DEFAULT_MAX_TURNS, History, HistoryLimits};

pub const SCRIPT_TOOL_NAME: &str = "bash";

pub const AGENT_CONFIG_TOOL_NAME: &str = "inspect_agent_config";

pub const ASSET_TOOL_NAME: &str = "fetch_chat_asset";

pub const DECLINE_REPLY_TOOL_NAME: &str = "decline_chat_reply";

const MAX_TOOL_CALLS_PER_TURN: usize = 10;

const MAX_TEXTUAL_ASSET_BYTES: usize = dekopon_shell::DEFAULT_MAX_OUTPUT_BYTES;
/// Capped at the shell's own output limit so a larger asset would end the session with a provider
/// context-length rejection instead of an answer.
const OPTIONAL_REPLY_INSTRUCTION: &str = "This message is an unaddressed continuation inside a \
chat thread the agent already owns. Reply when doing so would materially help. If no response is \
needed—for example, the people are talking to each other, acknowledged the result, or already \
resolved the point—call `decline_chat_reply` instead. That call posts nothing to chat. Do not reply \
merely to have the last word.";

const DECLINE_AFTER_WORK_RESULT: &str = "A chat reply is required because this session already \
invoked a capability. No tool calls from this turn were run. Provide a concise reply describing \
what happened instead.";

/// Returns no Result because a script failure is an outcome the model recovers from, like a nonzero
/// exit code, not a reason to end the session.
pub trait ScriptRuntime {
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome;

    fn command_words(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct FetchedAsset {
    pub name: String,
    pub mime: String,
    pub data: dekopon_model::asset::BlobReference,
}

impl fmt::Debug for FetchedAsset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FetchedAsset")
            .field("name", &self.name)
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

pub trait AssetSource {
    fn fetch(&self, id: u64) -> Result<FetchedAsset, String>;

    fn is_empty(&self) -> bool;
}

pub trait ModelUsageObserver: Send + Sync {
    fn observe(&self, usage: Option<ModelUsage>);
}

pub trait CancellationProbe: Send + Sync {
    fn is_cancelled(&self) -> bool;

    fn cancel_source(&self) -> Option<CancelSource> {
        None
    }
}

pub trait SteerSource: Send + Sync {
    /// Moves every queued steer out, oldest first, as finished user text. Empty when none.
    fn drain(&self) -> Vec<String>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptLimits {
    pub max_steps: u32,
    pub max_capability_calls: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyDisposition {
    Send,
    Suppress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOutcome {
    pub answer: String,
    pub disposition: ReplyDisposition,
    pub model_turns: u32,
    pub script_calls: u32,
    pub capability_invocations: u32,
    pub suggestions: Vec<ImprovementSuggestion>,
}

pub fn run_prompt<M, R>(
    model: &M,
    runtime: &R,
    prompt: &str,
    system: Option<&str>,
    limits: PromptLimits,
) -> Result<PromptOutcome, PromptError>
where
    M: ChatModel + ?Sized,
    R: ScriptRuntime + ?Sized,
{
    let mut history = History::default();
    run_prompt_with_history(model, runtime, prompt, system, limits, &mut history)
}

pub fn run_prompt_with_history<M, R>(
    model: &M,
    runtime: &R,
    prompt: &str,
    system: Option<&str>,
    limits: PromptLimits,
    history: &mut History,
) -> Result<PromptOutcome, PromptError>
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

pub fn run_prompt_with_history_and_options<M, R>(
    model: &M,
    runtime: &R,
    prompt: &str,
    system: Option<&str>,
    limits: PromptLimits,
    history: &mut History,
    options: &CompletionOptions,
) -> Result<PromptOutcome, PromptError>
where
    M: ChatModel + ?Sized,
    R: ScriptRuntime + ?Sized,
{
    run_prompt_session(
        model,
        runtime,
        SessionInputs::new(prompt, limits)
            .with_system(system)
            .with_options(options),
        history,
    )
}

pub struct SessionInputs<'a> {
    prompt: &'a str,
    system: Option<&'a str>,
    limits: PromptLimits,
    options: Option<&'a CompletionOptions>,
    assets: Option<&'a dyn AssetSource>,
    reply_assets: Option<&'a crate::attachment::ReplyAttachments>,
    usage_observer: Option<&'a dyn ModelUsageObserver>,
    agent_config: Option<&'a AgentConfigView>,
    cancellation: Option<&'a dyn CancellationProbe>,
    steering: Option<&'a dyn SteerSource>,
    progress: Option<Arc<dyn ProgressSink>>,
    optional_reply: bool,
    skills: &'a [Skill],
    improvement_suggestions: bool,
    wakes: Option<&'a dyn WakeRegistrar>,
}

impl<'a> SessionInputs<'a> {
    #[must_use]
    pub const fn new(prompt: &'a str, limits: PromptLimits) -> Self {
        Self {
            prompt,
            system: None,
            limits,
            options: None,
            assets: None,
            reply_assets: None,
            usage_observer: None,
            agent_config: None,
            cancellation: None,
            steering: None,
            progress: None,
            optional_reply: false,
            skills: &[],
            improvement_suggestions: false,
            wakes: None,
        }
    }

    #[must_use]
    pub const fn with_wakes(mut self, wakes: &'a dyn WakeRegistrar) -> Self {
        self.wakes = Some(wakes);
        self
    }

    #[must_use]
    pub const fn with_reply_assets(
        mut self,
        assets: &'a crate::attachment::ReplyAttachments,
    ) -> Self {
        self.reply_assets = Some(assets);
        self
    }

    #[must_use]
    pub const fn with_skills(mut self, skills: &'a [Skill]) -> Self {
        self.skills = skills;
        self
    }

    #[must_use]
    pub const fn with_improvement_suggestions(mut self) -> Self {
        self.improvement_suggestions = true;
        self
    }

    #[must_use]
    pub const fn with_system(mut self, system: Option<&'a str>) -> Self {
        self.system = system;
        self
    }

    #[must_use]
    pub const fn with_options(mut self, options: &'a CompletionOptions) -> Self {
        self.options = Some(options);
        self
    }

    #[must_use]
    pub const fn with_assets(mut self, assets: &'a dyn AssetSource) -> Self {
        self.assets = Some(assets);
        self
    }

    #[must_use]
    pub const fn with_usage_observer(mut self, observer: &'a dyn ModelUsageObserver) -> Self {
        self.usage_observer = Some(observer);
        self
    }

    #[must_use]
    pub const fn with_agent_config(mut self, config: &'a AgentConfigView) -> Self {
        self.agent_config = Some(config);
        self
    }

    #[must_use]
    pub const fn with_cancellation(mut self, cancellation: &'a dyn CancellationProbe) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    #[must_use]
    pub const fn with_steering(mut self, steering: &'a dyn SteerSource) -> Self {
        self.steering = Some(steering);
        self
    }

    #[must_use]
    pub fn with_progress(mut self, sink: Arc<dyn ProgressSink>) -> Self {
        self.progress = Some(sink);
        self
    }

    #[must_use]
    pub const fn with_optional_reply(mut self) -> Self {
        self.optional_reply = true;
        self
    }
}

#[derive(Clone, Copy)]
struct SessionExtensions<'a> {
    options: &'a CompletionOptions,
    assets: Option<&'a dyn AssetSource>,
    reply_assets: Option<&'a crate::attachment::ReplyAttachments>,
    usage_observer: Option<&'a dyn ModelUsageObserver>,
    agent_config: Option<&'a AgentConfigView>,
    cancellation: Option<&'a dyn CancellationProbe>,
    steering: Option<&'a dyn SteerSource>,
    progress: Option<&'a dyn ProgressSink>,
    optional_reply: bool,
    skills: &'a [Skill],
    improvement_suggestions: bool,
    wakes: Option<&'a dyn WakeRegistrar>,
}

pub fn run_prompt_session<M, R>(
    model: &M,
    runtime: &R,
    inputs: SessionInputs<'_>,
    history: &mut History,
) -> Result<PromptOutcome, PromptError>
where
    M: ChatModel + ?Sized,
    R: ScriptRuntime + ?Sized,
{
    let SessionInputs {
        prompt,
        system,
        limits,
        options,
        assets,
        reply_assets,
        usage_observer,
        agent_config,
        cancellation,
        steering,
        progress,
        optional_reply,
        skills,
        improvement_suggestions,
        wakes,
    } = inputs;
    let fallback = CompletionOptions::default();
    let options = options.unwrap_or(&fallback);
    let progress = progress.as_deref();
    if limits.max_steps == 0 {
        let error = PromptError::ZeroSteps;
        report_end(progress, cancellation, &error);
        return Err(error);
    }

    // Keep this message order fixed: instructions, skills listing, remembered history, then the
    // prompt, or prompt caching breaks.
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(ModelMessage::system(system));
    }
    if let Some(listing) = skills::prompt_block(skills) {
        messages.push(ModelMessage::system(listing));
    }
    if optional_reply {
        messages.push(ModelMessage::system(OPTIONAL_REPLY_INSTRUCTION));
    }
    history.replay_into(&mut messages);
    messages.push(ModelMessage::user(prompt));

    let (result, steers) = run_session(
        model,
        runtime,
        messages,
        limits,
        SessionExtensions {
            options,
            assets,
            reply_assets,
            usage_observer,
            agent_config,
            cancellation,
            steering,
            progress,
            optional_reply,
            skills,
            improvement_suggestions,
            wakes,
        },
    );
    let mut user = prompt.to_owned();
    for steer in steers {
        user.push_str("\n\n");
        user.push_str(&steer);
    }
    history.record(match &result {
        Ok(outcome) if outcome.disposition == ReplyDisposition::Send => {
            ConversationTurn::completed(user, outcome.answer.as_str())
        }
        Ok(_) | Err(_) => ConversationTurn::unanswered(user),
    });
    result
}

fn run_session<M, R>(
    model: &M,
    runtime: &R,
    messages: Vec<ModelMessage>,
    limits: PromptLimits,
    extensions: SessionExtensions<'_>,
) -> (Result<PromptOutcome, PromptError>, Vec<String>)
where
    M: ChatModel + ?Sized,
    R: ScriptRuntime + ?Sized,
{
    let mut steers = Vec::new();
    let result = run_turns(model, runtime, messages, limits, extensions, &mut steers);
    if let Err(error) = &result {
        report_end(extensions.progress, extensions.cancellation, error);
    }
    (result, steers)
}

fn report_end(
    progress: Option<&dyn ProgressSink>,
    cancellation: Option<&dyn CancellationProbe>,
    error: &PromptError,
) {
    let Some(sink) = progress else {
        return;
    };
    sink.emit(match FailureClass::of(error) {
        Some(class) => ProgressEvent::Failed { class },
        None => ProgressEvent::Cancelled {
            by: cancellation
                .and_then(CancellationProbe::cancel_source)
                .unwrap_or(CancelSource::Operator),
        },
    });
}

fn emit(progress: Option<&dyn ProgressSink>, event: ProgressEvent) {
    if let Some(sink) = progress {
        sink.emit(event);
    }
}

fn drain_steers(
    steering: Option<&dyn SteerSource>,
    messages: &mut Vec<ModelMessage>,
    consumed: &mut Vec<String>,
) -> bool {
    let steers = steering.map(SteerSource::drain).unwrap_or_default();
    let any = !steers.is_empty();
    for steer in steers {
        messages.push(ModelMessage::user(&steer));
        consumed.push(steer);
    }
    any
}

fn run_turns<M, R>(
    model: &M,
    runtime: &R,
    mut messages: Vec<ModelMessage>,
    limits: PromptLimits,
    extensions: SessionExtensions<'_>,
    steers: &mut Vec<String>,
) -> Result<PromptOutcome, PromptError>
where
    M: ChatModel + ?Sized,
    R: ScriptRuntime + ?Sized,
{
    let SessionExtensions {
        options,
        assets,
        reply_assets,
        usage_observer,
        agent_config,
        cancellation,
        steering,
        progress,
        optional_reply,
        skills,
        improvement_suggestions,
        wakes,
    } = extensions;
    let mut model_tools = vec![script_tool(&runtime.command_words())];
    if agent_config.is_some() {
        model_tools.push(agent_config_tool());
    }
    if !skills.is_empty() {
        model_tools.push(skills::skill_tool());
    }
    if improvement_suggestions {
        model_tools.push(improvement::improvement_tool());
    }
    if optional_reply {
        model_tools.push(decline_reply_tool());
    }
    if wakes.is_some() {
        model_tools.push(wake::wake_tool());
    }

    let session_span = tracing::info_span!(
        "prompt.session",
        prompt.max_steps = limits.max_steps,
        prompt.max_capability_calls = limits.max_capability_calls
    );
    let _session = session_span.enter();
    let session_started = Instant::now();
    let mut script_calls = 0_u32;
    let mut capability_invocations = 0_u32;
    let mut tool_calls = 0_u32;
    let mut transcribed = 0_usize;
    let mut agent_config_shown = false;
    let mut skill_reads = SkillReads::default();
    let mut suggestions = Vec::new();

    let mut completed_turns = 0;
    loop {
        if completed_turns == limits.max_steps {
            return Err(PromptError::MaxSteps {
                maximum: limits.max_steps,
            });
        }
        let model_turns = completed_turns + 1;
        check_cancelled(cancellation)?;
        drain_steers(steering, &mut messages, steers);
        model_tools.retain(|tool| tool.name != ASSET_TOOL_NAME);
        if assets.is_some_and(|source| !source.is_empty()) {
            model_tools.push(asset_tool());
        }
        let model_span = tracing::info_span!(
            "prompt.model_turn",
            model.turn = model_turns,
            usage.input_tokens = tracing::field::Empty,
            usage.cached_input_tokens = tracing::field::Empty,
            usage.cache_write_tokens = tracing::field::Empty,
            usage.output_tokens = tracing::field::Empty,
            usage.reasoning_output_tokens = tracing::field::Empty,
            usage.total_tokens = tracing::field::Empty,
            stream.deltas = tracing::field::Empty,
            stream.first_delta_ms = tracing::field::Empty,
        );
        let model_entered = model_span.enter();
        let scope = if transcribed == 0 { "full" } else { "delta" };
        tracing::info!(
            target: "dekopon_agent::audit",
            {
                audit.event = "agent.model.prompt",
                model.turn = model_turns,
                transcript.scope = scope,
                message.count = messages.len(),
                messages = %transcript(&messages[transcribed..]),
            },
            "model turn prompt"
        );
        transcribed = messages.len();
        let model_started = Instant::now();
        emit(
            progress,
            ProgressEvent::ModelTurn {
                turn: model_turns,
                of: limits.max_steps,
            },
        );
        let mut stream = TurnStream::new(model_turns, model_started, progress, cancellation);
        let completion = {
            let mut on_event = |event| stream.observe(event);
            model.complete(&messages, &model_tools, options, &mut on_event)
        };
        stream.record_on(&model_span);
        let turn = match completion {
            Ok(turn) => turn,
            Err(InferenceError::Cancelled) => {
                let steered = steering.is_some()
                    && !cancellation.is_some_and(CancellationProbe::is_cancelled);
                tracing::info!(
                    target: "dekopon_agent::audit",
                    {
                        audit.event = "accounting.model.turn",
                        model.turn = model_turns,
                        duration_ms = milliseconds(model_started.elapsed()),
                        message.count = messages.len(),
                        outcome = if steered { "steered" } else { "interrupted" },
                    },
                    "model turn interrupted"
                );
                tracing::info!(
                    target: "dekopon_agent::audit",
                    {
                        audit.event = "agent.model.answer",
                        model.turn = model_turns,
                        answer = stream.text().as_str(),
                        tool_calls = %tool_calls_json(&[]),
                        stream.interrupted = true,
                    },
                    "model turn answer"
                );
                drop(model_entered);
                if steered {
                    emit(progress, ProgressEvent::Steered { turn: model_turns });
                    continue;
                }
                return Err(PromptError::Cancelled);
            }
            Err(error) => {
                tracing::error!(
                    target: "dekopon_agent::audit",
                    {
                        audit.event = "accounting.model.turn",
                        model.turn = model_turns,
                        duration_ms = milliseconds(model_started.elapsed()),
                        outcome = "failed",
                        error = %error,
                    },
                    "model turn failed"
                );
                return Err(error.into());
            }
        };
        completed_turns = model_turns;
        if let Some(observer) = usage_observer {
            observer.observe(turn.usage);
        }
        if let Some(usage) = &turn.usage {
            record_usage(&model_span, usage);
        }
        tracing::info!(
            target: "dekopon_agent::audit",
            {
                audit.event = "accounting.model.turn",
                model.turn = model_turns,
                duration_ms = milliseconds(model_started.elapsed()),
                message.count = messages.len(),
                tool_call.count = turn.tool_calls.len(),
                usage.input_tokens = turn.usage.as_ref().and_then(|usage| usage.input_tokens),
                usage.cached_input_tokens = turn.usage.as_ref().and_then(|usage| usage.cached_input_tokens),
                usage.cache_write_tokens = turn.usage.as_ref().and_then(|usage| usage.cache_write_tokens),
                usage.output_tokens = turn.usage.as_ref().and_then(|usage| usage.output_tokens),
                usage.reasoning_output_tokens = turn.usage.as_ref().and_then(|usage| usage.reasoning_output_tokens),
                usage.total_tokens = turn.usage.as_ref().and_then(|usage| usage.total_tokens),
                answer.present = turn
                    .content
                    .as_ref()
                    .is_some_and(|content| !content.trim().is_empty()),
                outcome = "succeeded",
            },
            "model turn accounted"
        );
        tracing::info!(
            target: "dekopon_agent::audit",
            {
                audit.event = "agent.model.answer",
                model.turn = model_turns,
                answer = turn.content.as_deref().unwrap_or_default(),
                tool_calls = %tool_calls_json(&turn.tool_calls),
            },
            "model turn answer"
        );
        let requested = u32::try_from(turn.tool_calls.len()).unwrap_or(u32::MAX);
        tool_calls = tool_calls.saturating_add(requested);
        emit(
            progress,
            ProgressEvent::Answered {
                turn: model_turns,
                tool_calls: requested,
                duration: model_started.elapsed(),
                first_delta: stream.first_delta(),
            },
        );
        drop(model_entered);
        check_cancelled(cancellation)?;
        messages.push(assistant_message(&turn));

        if turn.tool_calls.is_empty() {
            check_cancelled(cancellation)?;
            if model_turns < limits.max_steps && drain_steers(steering, &mut messages, steers) {
                continue;
            }
            let answer = turn
                .content
                .filter(|content| !content.trim().is_empty())
                .or_else(|| {
                    reply_assets
                        .filter(|assets| assets.has_queued())
                        .map(|_| String::new())
                })
                .ok_or(PromptError::EmptyAnswer)?;
            emit(
                progress,
                ProgressEvent::Finished {
                    outcome: SessionOutcome::Answered,
                    elapsed: session_started.elapsed(),
                    turns: model_turns,
                    tool_calls,
                },
            );
            return Ok(PromptOutcome {
                answer,
                disposition: ReplyDisposition::Send,
                model_turns,
                script_calls,
                capability_invocations,
                suggestions,
            });
        }
        if turn.tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
            tracing::error!(
                target: "dekopon_agent::audit",
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

        let decline_requested = optional_reply
            && turn
                .tool_calls
                .iter()
                .any(|call| call.function.name == DECLINE_REPLY_TOOL_NAME);
        if decline_requested {
            for (index, call) in turn.tool_calls.iter().enumerate() {
                if call.id.as_str().trim().is_empty() {
                    reject_tool_call(model_turns, index + 1, "empty-tool-call-id");
                    return Err(PromptError::EmptyToolCallId);
                }
                if call.function.name == DECLINE_REPLY_TOOL_NAME {
                    decline_reply_argument(&call.function.name, &call.function.arguments)?;
                }
            }
            if capability_invocations == 0 {
                check_cancelled(cancellation)?;
                tracing::info!(
                    target: "dekopon_agent::audit",
                    {
                        audit.event = "agent.reply.declined",
                        model.turn = model_turns,
                    },
                    "optional chat reply declined"
                );
                emit(
                    progress,
                    ProgressEvent::Finished {
                        outcome: SessionOutcome::Declined,
                        elapsed: session_started.elapsed(),
                        turns: model_turns,
                        tool_calls,
                    },
                );
                return Ok(PromptOutcome {
                    answer: String::new(),
                    disposition: ReplyDisposition::Suppress,
                    model_turns,
                    script_calls,
                    capability_invocations,
                    suggestions,
                });
            }

            // Once a capability has run, the model cannot decline silently, since silence could
            // conceal an effect that capability already had outside the conversation.
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

        for (tool_call_index, call) in turn.tool_calls.into_iter().enumerate() {
            check_cancelled(cancellation)?;
            let tool_call_index = tool_call_index + 1;
            if call.id.as_str().trim().is_empty() {
                reject_tool_call(model_turns, tool_call_index, "empty-tool-call-id");
                return Err(PromptError::EmptyToolCallId);
            }
            if call.function.name == AGENT_CONFIG_TOOL_NAME
                && let Some(config) = agent_config
            {
                inspect_agent_config_into(
                    &mut messages,
                    config,
                    &call,
                    model_turns,
                    tool_call_index,
                    &mut agent_config_shown,
                )?;
                continue;
            }
            if call.function.name == SKILL_TOOL_NAME && !skills.is_empty() {
                skills::read_skill_into(
                    &mut messages,
                    skills,
                    &mut skill_reads,
                    &call,
                    model_turns,
                    tool_call_index,
                )?;
                continue;
            }
            if call.function.name == IMPROVEMENT_TOOL_NAME && improvement_suggestions {
                improvement::suggest_improvement_into(
                    &mut messages,
                    &mut suggestions,
                    &call,
                    model_turns,
                    tool_call_index,
                )?;
                continue;
            }
            if call.function.name == WAKE_TOOL_NAME
                && let Some(registrar) = wakes
            {
                wake::wake_into(
                    &mut messages,
                    registrar,
                    &call,
                    model_turns,
                    tool_call_index,
                )?;
                continue;
            }
            if call.function.name == ASSET_TOOL_NAME
                && let Some(source) = assets
            {
                fetch_asset_into(&mut messages, source, &call, model_turns, tool_call_index)?;
                continue;
            }
            // The model-selected tool name is excluded from telemetry because it is untrusted model
            // output; an operator reads it from stderr instead.
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

            // Remaining budget is computed from what the session already spent, so a model cannot
            // widen its own capability budget by splitting work across more scripts.
            let remaining = limits
                .max_capability_calls
                .saturating_sub(capability_invocations);
            let span = tracing::info_span!(
                "prompt.script",
                model.turn = model_turns,
                tool_call.index = tool_call_index,
                script.max_capability_calls = remaining,
                script.bytes = script.len()
            );
            let outcome = {
                let _entered = span.enter();
                tracing::info!(
                    target: "dekopon_agent::audit",
                    {
                        audit.event = "agent.tool.script",
                        model.turn = model_turns,
                        tool_call.index = tool_call_index,
                        script = script.as_str(),
                    },
                    "agent tool script"
                );
                check_cancelled(cancellation)?;
                let outcome = runtime.run_script(&script, remaining);
                check_cancelled(cancellation)?;
                tracing::info!(
                    target: "dekopon_agent::audit",
                    {
                        audit.event = "agent.tool.output",
                        model.turn = model_turns,
                        tool_call.index = tool_call_index,
                        output = outcome.output.as_str(),
                    },
                    "agent tool output"
                );
                outcome
            };
            script_calls = script_calls.saturating_add(1);
            capability_invocations =
                capability_invocations.saturating_add(outcome.capability_calls);
            messages.push(ModelMessage::tool(call.id, format_script_outcome(&outcome)));
        }
    }
}

fn check_cancelled(cancellation: Option<&dyn CancellationProbe>) -> Result<(), PromptError> {
    if cancellation.is_some_and(CancellationProbe::is_cancelled) {
        Err(PromptError::Cancelled)
    } else {
        Ok(())
    }
}

struct TurnStream<'a> {
    turn: u32,
    started: Instant,
    progress: Option<&'a dyn ProgressSink>,
    cancellation: Option<&'a dyn CancellationProbe>,
    text: ModelText,
    chars: usize,
    deltas: u64,
    first_delta: Option<Duration>,
    bound_passed: bool,
}

impl<'a> TurnStream<'a> {
    fn new(
        turn: u32,
        started: Instant,
        progress: Option<&'a dyn ProgressSink>,
        cancellation: Option<&'a dyn CancellationProbe>,
    ) -> Self {
        Self {
            turn,
            started,
            progress,
            cancellation,
            text: ModelText::default(),
            chars: 0,
            deltas: 0,
            first_delta: None,
            bound_passed: false,
        }
    }

    fn observe(&mut self, event: TurnEvent) -> ControlFlow<()> {
        match event {
            TurnEvent::TextDelta(delta) => self.append(delta),
            TurnEvent::ToolCallStarted { .. } => {}
        }
        if self
            .cancellation
            .is_some_and(CancellationProbe::is_cancelled)
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    fn append(&mut self, delta: ModelText) {
        self.deltas = self.deltas.saturating_add(1);
        if self.first_delta.is_none() {
            self.first_delta = Some(self.started.elapsed());
        }
        self.chars = self.chars.saturating_add(delta.as_str().chars().count());
        self.text.push(&delta);
        if self.bound_passed {
            return;
        }
        if self.text.len() > STREAMED_TEXT_BOUND_BYTES {
            self.bound_passed = true;
            return;
        }
        emit(
            self.progress,
            ProgressEvent::TextDelta {
                turn: self.turn,
                text: delta,
                cumulative_chars: self.chars,
            },
        );
    }

    const fn text(&self) -> &ModelText {
        &self.text
    }

    const fn first_delta(&self) -> Option<Duration> {
        self.first_delta
    }

    fn record_on(&self, span: &tracing::Span) {
        span.record("stream.deltas", self.deltas);
        if let Some(first) = self.first_delta {
            span.record("stream.first_delta_ms", milliseconds(first));
        }
    }
}

#[must_use]
pub fn format_script_outcome(outcome: &ScriptOutcome) -> String {
    let mut text = outcome.output.clone();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&format!("[exit code: {}]", outcome.exit_code));
    text
}

pub(crate) fn reject_tool_call(model_turn: u32, tool_call_index: usize, error_type: &'static str) {
    tracing::error!(
        target: "dekopon_agent::audit",
        {
            audit.event = "agent.tool.rejected",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            error.type = error_type,
        },
        "model tool call rejected"
    );
}

fn script_tool(command_words: &[String]) -> ModelTool {
    let mut description = SCRIPT_TOOL_DESCRIPTION.to_owned();
    if !command_words.is_empty() {
        let mut words = command_words.to_vec();
        words.sort();
        words.dedup();
        description.push_str(&format!(
            "\n\nThis session's providers add these command words: {}.",
            words.join(", ")
        ));
    }
    ModelTool {
        name: SCRIPT_TOOL_NAME.to_owned(),
        description,
        parameters: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The script to run. Multiple lines are expected and encouraged."
                }
            },
            "required": ["script"],
            "additionalProperties": false
        }),
    }
}

fn decline_reply_tool() -> ModelTool {
    ModelTool {
        name: DECLINE_REPLY_TOOL_NAME.to_owned(),
        description: "Post nothing to chat and end this optional continuation. Call this instead \
                      of writing text when a reply would not materially help or would merely take \
                      the last word. Call it before running capabilities; once capability work has \
                      happened, a concise report is required."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

fn agent_config_tool() -> ModelTool {
    ModelTool {
        name: AGENT_CONFIG_TOOL_NAME.to_owned(),
        description: "Inspect this session's credential-free agent configuration. Call this when \
                      asked about the agent's prompt, configuration, Cedar policy, permissions, \
                      tools, limits, or memory. The result contains the exact standing \
                      instructions, route/session bounds, and only the capabilities Cedar \
                      currently grants this sender through this agent. Present it as concise \
                      Markdown tables unless raw JSON was requested. Raw Cedar source, policy \
                      identifiers, principals, subjects, endpoints, paths, legacy credential \
                      names, private secret-map inventory, and all credential values are \
                      intentionally omitted. A public DRN may appear only when the operator put \
                      that inert name in the standing instructions."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

const AGENT_CONFIG_ALREADY_SHOWN: &str = "This session's agent configuration is already in this \
                                          conversation, in the earlier inspect_agent_config \
                                          result. It cannot change within a session; read that \
                                          result again.";

fn inspect_agent_config_into(
    messages: &mut Vec<ModelMessage>,
    config: &AgentConfigView,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
    already_shown: &mut bool,
) -> Result<(), PromptError> {
    if let Err(error) = agent_config_argument(&call.function.name, &call.function.arguments) {
        reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
        return Err(error);
    }
    let result = if *already_shown {
        AGENT_CONFIG_ALREADY_SHOWN.to_owned()
    } else {
        config.tool_result()
    };
    tracing::info!(
        target: "dekopon_agent::audit",
        {
            audit.event = "agent.config.inspected",
            model.turn = model_turn,
            tool_call.index = tool_call_index,
            config.bytes = result.len(),
            config.repeated = *already_shown,
        },
        "agent configuration inspected"
    );
    *already_shown = true;
    messages.push(ModelMessage::tool(call.id.clone(), result));
    Ok(())
}

fn decline_reply_argument(tool: &str, arguments: &str) -> Result<(), PromptError> {
    let arguments = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    let Value::Object(arguments) = arguments else {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    };
    if !arguments.is_empty() {
        return Err(PromptError::DeclineReplyArgumentsNotEmpty {
            tool: tool.to_owned(),
        });
    }
    Ok(())
}

fn agent_config_argument(tool: &str, arguments: &str) -> Result<(), PromptError> {
    let arguments = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    let Value::Object(arguments) = arguments else {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    };
    if !arguments.is_empty() {
        return Err(PromptError::AgentConfigArgumentsNotEmpty {
            tool: tool.to_owned(),
        });
    }
    Ok(())
}

fn is_textual(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/xml" | "application/x-yaml" | "application/yaml"
        )
}

fn asset_tool() -> ModelTool {
    ModelTool {
        name: ASSET_TOOL_NAME.to_owned(),
        description: "Look at an inbound or generated file in this conversation. The conversation \
                      names each one as `Chat Asset #N`; pass that number. Call this when \
                      answering depends on what the file actually contains."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "The number from the `Chat Asset #N` reference in the conversation."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
    }
}

/// A tool result cannot carry an attachment on either wire format, so the asset always arrives as a
/// following message; this shape must not be simplified.
fn fetch_asset_into(
    messages: &mut Vec<ModelMessage>,
    source: &dyn AssetSource,
    call: &ModelToolCall,
    model_turn: u32,
    tool_call_index: usize,
) -> Result<(), PromptError> {
    let id = match asset_argument(&call.function.name, &call.function.arguments) {
        Ok(id) => id,
        Err(error) => {
            reject_tool_call(model_turn, tool_call_index, error.telemetry_kind());
            return Err(error);
        }
    };
    let span = tracing::info_span!(
        "prompt.asset_fetch",
        model.turn = model_turn,
        tool_call.index = tool_call_index,
        asset.id = id,
    );
    let _entered = span.enter();
    let asset = match source.fetch(id) {
        Ok(asset) => asset,
        Err(reason) => {
            tracing::info!(
                target: "dekopon_agent::audit",
                { audit.event = "agent.asset.refused", asset.id = id, reason = reason.as_str() },
                "chat asset refused"
            );
            messages.push(ModelMessage::tool(call.id.clone(), reason));
            return Ok(());
        }
    };
    let text = if is_textual(&asset.mime) {
        match asset.data.read() {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(error) => {
                messages.push(ModelMessage::tool(call.id.clone(), error.to_string()));
                return Ok(());
            }
        }
    } else {
        None
    };
    let truncated = text
        .as_ref()
        .is_some_and(|text| text.len() > MAX_TEXTUAL_ASSET_BYTES);
    // Only size and media type are logged, never the bytes or the sender's file name, since the
    // file name is untrusted.
    tracing::info!(
        target: "dekopon_agent::audit",
        {
            audit.event = "agent.asset.fetched",
            asset.id = id,
            asset.mime = asset.mime.as_str(),
            asset.bytes = asset.data.len(),
            asset.truncated = truncated,
        },
        "chat asset fetched"
    );
    if let Some(text) = text {
        messages.push(ModelMessage::tool(
            call.id.clone(),
            clamp_textual_asset(text),
        ));
        return Ok(());
    }
    messages.push(ModelMessage::tool(
        call.id.clone(),
        format!("Chat Asset #{id} follows in the next message."),
    ));
    let part = if asset.mime.starts_with("image/") {
        ContentPart::Image {
            mime: asset.mime,
            data: asset.data,
        }
    } else {
        ContentPart::File {
            name: asset.name,
            mime: asset.mime,
            data: asset.data,
        }
    };
    messages.push(ModelMessage::user_with_parts(vec![
        ContentPart::Text(format!("Chat Asset #{id}:")),
        part,
    ]));
    Ok(())
}

fn clamp_textual_asset(mut text: String) -> String {
    let total = text.len();
    if total <= MAX_TEXTUAL_ASSET_BYTES {
        return text;
    }
    let mut end = MAX_TEXTUAL_ASSET_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!("\n[truncated at {end} bytes of {total}]"));
    text
}

fn asset_argument(tool: &str, arguments: &str) -> Result<u64, PromptError> {
    let arguments = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    let Value::Object(arguments) = arguments else {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    };
    let id = arguments.get("id").and_then(|id| match id {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    });
    id.ok_or_else(|| PromptError::MissingAssetId {
        tool: tool.to_owned(),
    })
}

fn script_argument(tool: &str, arguments: &str) -> Result<String, PromptError> {
    let arguments = serde_json::from_str::<Value>(arguments).map_err(|source| {
        PromptError::InvalidArguments {
            tool: tool.to_owned(),
            source,
        }
    })?;
    let Value::Object(arguments) = arguments else {
        return Err(PromptError::ArgumentsNotObject {
            tool: tool.to_owned(),
        });
    };
    match arguments.get("script") {
        Some(Value::String(script)) => Ok(script.clone()),
        _ => Err(PromptError::MissingScript {
            tool: tool.to_owned(),
        }),
    }
}

const SCRIPT_TOOL_DESCRIPTION: &str = "\
Run one script in Dekopon's sandboxed shell. This is the only way to invoke capabilities: use it \
whenever the task needs data or an action the session's capabilities provide, and write the whole \
job as one script rather than one tool call per step. Send scripts one after another only when \
the next step genuinely depends on a result you cannot know yet. Returns the script's combined \
output followed by an `[exit code: N]` trailer, exactly as a terminal would.

The dialect is eerily close to bash and explicitly not bash. Pipelines, `&&`, `||`, `;`, a \
leading `!`, `if`/`elif`/`else`, `for`, `while`, `until`, `case`/`esac`, `[[ ... ]]`, `{ ...; }` \
groups — the compound ones all usable as pipeline stages, so `cmd | while ...; do ...; done` \
works and a piped loop keeps what it assigns because nothing here forks — `break`/`continue`, \
functions with `$1`/`$@`/`$#`/`shift`/`local`, `read`, `$NAME`, `${NAME[index]}`, \
`${NAME[@]}`, `${#NAME}`, `${NAME:-default}` and its `:=`/`:?`/`:+`/`#`/`%`/`/` relatives, `$( \
)`, `$(( ))`, `$?`, `${PIPESTATUS[@]}`, `set -e`/`set -u`/`set -o pipefail`, `return`, `exit`, \
both quoting forms, here-documents (`<<EOF`, `<<-EOF`, and literal `<<'EOF'`), and redirection of \
either stream (`>`, `>>`, `2>`, `2>>`, `&>`, `2>&1`, `>&2`, `> /dev/null`) into named in-memory \
buffers all behave the way you expect. Everything outside that curated set fails loudly and by \
name: `eval`, backticks, subshells, `<<<`, and `&` backgrounding are errors, never silent no-ops. \
If a script ran, it did what it said.

Four things genuinely differ from a real shell:

1. There are no processes, no filesystem, no environment variables, and no network reachable \
except through a capability. The capabilities you may invoke are exactly those this session was \
granted: no flag, retry, or rewording escalates past that set, and a refusal is a fact to report, \
not an obstacle to work around.
2. Provider command words are programs. A provider adds words of its own, and each behaves like a \
command-line tool with subcommands and flags: run `<word> --help` to learn one before using it. \
Its subcommands call capabilities on your behalf, so a word can do only what this session was \
granted, and `cap --list` shows those capability IDs.
3. Values are JSON, not text. `|` hands a structured value to the next command, and `jq` is built \
in to work on it. A command writes its value to stdout and its diagnostics to stderr, so \
`x=$(cmd)` captures the value while errors still reach you, and `x=$(cmd 2>&1)` is how you \
capture the error text itself. Merging only happens when there is a diagnostic: `cmd 2>&1` on a \
quiet command leaves its value, and its type, untouched.
4. The session is bounded. Steps, output, wall-clock time, and capability calls all have \
ceilings; tripping one ends the script with a message naming it. Filter with `jq`, loop in the \
shell, and print only what you need next.

Builtins: `jq`, `cap`, `cat`, `echo`, `printf`, `test`/`[`, `true`, `false`, `sleep`, `grep`, \
`sed`, `cut`, `sort`, `uniq`, `wc`, `base64`, `xargs`. Any provider command words this session has \
are listed at the end of this description.

A public secret DRN supplied in your instructions is a name, not a value or grant. Pass one only to \
a provider command whose `--help` says it accepts one: the command proposes using that secret, the \
broker independently authorizes every use, and neither you nor the provider ever reads the secret \
itself.

Patterns are literal text, never globs, and regular expressions only where you ask for one with \
`-E`: a `grep`/`sed` pattern, a `${NAME#p}`/`${NAME%p}`/`${NAME/p/r}` pattern, the right operand \
of `==` inside `[[ ]]`, and a `case` pattern too, where `*)` remains the default branch but \
`*.json)` is an error rather than a silent mismatch. `grep -E '[0-9]'` and `sed -E 's/^ *//'` are \
how you get a real regular expression, and the only way: unflagged, both are a usage error naming \
the metacharacter rather than a search that quietly finds nothing. Under `-E`, anchors and `.` \
mean what they mean in any regex, but the replacement half of `sed` is still literal text, so \
groups select and do not substitute. `${#NAME}` counts characters of a string but elements of an \
array and keys of an object, because values here are real JSON. Use `jq` when the thing you want \
is structure rather than lines. A here-document's body arrives as one JSON string, so pipe it \
through `jq fromjson` when you want structure out of it.

Reading the result. The tool result is your only evidence: what a script printed is what you \
know, and what it did not print you do not know, so never guess what a capability returned, what \
it accepts, or whether it exists. Exit 0 is success. Exit 1 is a command that ran and failed; \
its error arrives on stderr as `<name>: failed: ...`, naming the command or the capability it \
called, so read it before retrying. Exit 127 means the word is not a builtin or a command this \
session's providers add (`command not found`), or that a command needs a capability this session \
was not granted, and the message says which; guessing at more names will not change either. Exit \
126 means this session holds the capability \
but authorization refused this use; different arguments will not change that, so report it. Exit \
2 is a parse error, a refused construct, a usage error, or an exhausted budget, and the message \
names which. Exit 124 is the wall-clock deadline. Output past the ceiling is truncated in the \
middle, keeping the head and the tail with a marker giving the total line count, so filter inside \
the script rather than printing everything and reading it here. Each script starts empty: nothing \
an earlier script assigned survives, but everything it printed is already in this conversation.

Not for: skills, chat attachments, and this agent's own configuration are not files here, and \
when the session offers a tool for one of them it is listed beside this one. There are no files \
at all: `ls` and `cd` do not exist, and `cat` only passes along what is piped or here-documented \
into it.

There is no `help` builtin. Discover once, then prefer a single script that does the whole job \
over many small ones — that is the entire point of this tool.";

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("prompt session was cancelled")]
    Cancelled,
    #[error("prompt max steps must be greater than zero")]
    ZeroSteps,
    #[error(transparent)]
    Model(#[from] InferenceError),
    #[error("model requested unknown or unavailable tool {0:?}")]
    UnknownTool(String),
    #[error("model returned {actual} tool calls in one turn; the maximum is {maximum}")]
    TooManyToolCalls { actual: usize, maximum: usize },
    #[error("model returned an empty tool-call ID")]
    EmptyToolCallId,
    #[error("model returned invalid JSON arguments for tool {tool:?}")]
    InvalidArguments {
        tool: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("model arguments for tool {tool:?} must be a JSON object")]
    ArgumentsNotObject { tool: String },
    #[error("model arguments for tool {tool:?} must be an empty object")]
    AgentConfigArgumentsNotEmpty { tool: String },
    #[error("model arguments for tool {tool:?} must be an empty object")]
    DeclineReplyArgumentsNotEmpty { tool: String },
    #[error("model arguments for tool {tool:?} must include a string \"script\" field")]
    MissingScript { tool: String },
    #[error("model arguments for tool {tool:?} must include an integer \"id\" field")]
    MissingAssetId { tool: String },
    #[error("model arguments for tool {tool:?} must include a non-empty string \"name\" field")]
    MissingSkillName { tool: String },
    #[error("model arguments for tool {tool:?} contain unexpected or mistyped fields")]
    UnexpectedSkillArguments { tool: String },
    #[error("model arguments for tool {tool:?} do not match the suggestion schema")]
    InvalidSuggestion {
        tool: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("model arguments for tool {tool:?} do not match the wake schema")]
    InvalidWake {
        tool: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("model tried to suppress a reply after capability work with no reporting turn left")]
    UnreportedCapabilityWork,
    #[error("model returned neither tool calls nor a final answer")]
    EmptyAnswer,
    #[error("model did not produce a final answer within {maximum} turns")]
    MaxSteps { maximum: u32 },
}

impl PromptError {
    #[must_use]
    pub fn telemetry_kind(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::ZeroSteps => "zero-steps",
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
            Self::MissingSkillName { .. } => "missing-skill-name",
            Self::UnexpectedSkillArguments { .. } => "unexpected-skill-arguments",
            Self::InvalidSuggestion { .. } => "invalid-suggestion",
            Self::InvalidWake { .. } => "invalid-wake",
            Self::UnreportedCapabilityWork => "unreported-capability-work",
            Self::EmptyAnswer => "empty-answer",
            Self::MaxSteps { .. } => "max-steps",
        }
    }
}

fn record_usage(span: &tracing::Span, usage: &ModelUsage) {
    if let Some(tokens) = usage.input_tokens {
        span.record("usage.input_tokens", tokens);
    }
    if let Some(tokens) = usage.cached_input_tokens {
        span.record("usage.cached_input_tokens", tokens);
    }
    if let Some(tokens) = usage.cache_write_tokens {
        span.record("usage.cache_write_tokens", tokens);
    }
    if let Some(tokens) = usage.output_tokens {
        span.record("usage.output_tokens", tokens);
    }
    if let Some(tokens) = usage.reasoning_output_tokens {
        span.record("usage.reasoning_output_tokens", tokens);
    }
    if let Some(tokens) = usage.total_tokens {
        span.record("usage.total_tokens", tokens);
    }
}

fn transcript(messages: &[ModelMessage]) -> String {
    serde_json::to_string(messages).unwrap_or_else(|_| "<unserializable>".to_owned())
}

fn tool_calls_json(tool_calls: &[ModelToolCall]) -> String {
    serde_json::to_string(tool_calls).unwrap_or_else(|_| "<unserializable>".to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        ops::ControlFlow,
        sync::Arc,
        sync::Mutex,
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    };

    use dekopon_model::TurnEvent;
    use dekopon_model::error::InferenceError;
    use dekopon_model::model::{
        AssistantTurn, ChatModel, CompletionOptions, ModelFunctionCall, ModelMessage, ModelTool,
        ModelToolCall, ModelUsage,
    };
    use dekopon_shell::{
        CapabilityCallResult, CapabilityInvoker, CommandRun, ExitCode, ScriptOutcome,
    };
    use serde_json::{Value, json};

    use crate::meta::{
        AgentConfigView, EffectiveCapabilityView, MemoryConfigView, SessionConfigView,
    };
    use crate::progress::{
        CancelSource, CancelVia, ProgressEvent, ProgressSink, STREAMED_TEXT_BOUND_BYTES,
    };

    use super::{
        AGENT_CONFIG_ALREADY_SHOWN, AGENT_CONFIG_TOOL_NAME, ASSET_TOOL_NAME, AssetSource,
        CancellationProbe, ConversationTurn, DECLINE_REPLY_TOOL_NAME, DEFAULT_MAX_BYTES,
        DEFAULT_MAX_TURNS, FetchedAsset, History, HistoryLimits, IMPROVEMENT_TOOL_NAME,
        MAX_TEXTUAL_ASSET_BYTES, MAX_TOOL_CALLS_PER_TURN, ModelUsageObserver, PromptError,
        PromptLimits, ReplyDisposition, SCRIPT_TOOL_DESCRIPTION, SCRIPT_TOOL_NAME, SKILL_TOOL_NAME,
        ScriptRuntime, SessionInputs, SteerSource, agent_config_tool, format_script_outcome,
        run_prompt, run_prompt_session, run_prompt_with_history,
        run_prompt_with_history_and_options, script_tool,
    };

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

        fn first_request(&self) -> Vec<ModelMessage> {
            self.observed_messages
                .lock()
                .expect("message observations lock")
                .first()
                .cloned()
                .expect("the model was asked at least once")
        }

        fn first_roles(&self) -> Vec<(&'static str, String)> {
            self.first_request()
                .iter()
                .map(|message| {
                    (
                        message.role(),
                        message.content().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        }

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
            _options: &CompletionOptions,
            _on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
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
                .ok_or(InferenceError::Protocol(
                    dekopon_model::error::ProtocolFailure::NoChoices,
                ))
        }
    }

    struct RecordingRuntime {
        scripts: Mutex<Vec<(String, u32)>>,
        capability_calls_per_script: u32,
        steers: Option<Arc<QueuedSteers>>,
    }

    impl RecordingRuntime {
        fn new(capability_calls_per_script: u32) -> Self {
            Self {
                scripts: Mutex::new(Vec::new()),
                capability_calls_per_script,
                steers: None,
            }
        }
    }

    impl ScriptRuntime for RecordingRuntime {
        fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome {
            self.scripts
                .lock()
                .expect("script lock")
                .push((script.to_owned(), max_capability_calls));
            if let Some(steers) = &self.steers {
                steers.push("msg2");
            }
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
        AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: id.into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": script }).to_string(),
                },
            }],
            None,
        )
    }

    fn answer(text: &str) -> AssistantTurn {
        AssistantTurn::new(Some(text.to_owned()), Vec::new(), None)
    }

    fn decline_call(id: &str, arguments: Value) -> ModelToolCall {
        ModelToolCall {
            id: id.into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: DECLINE_REPLY_TOOL_NAME.to_owned(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn decline(arguments: Value) -> AssistantTurn {
        AssistantTurn::new(None, vec![decline_call("decline-call", arguments)], None)
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
            SessionInputs::new("stop", limits(2, 2)).with_cancellation(&Cancelled),
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
                .map(progress_label)
                .collect()
        }

        fn forwarded_chars(&self) -> Vec<usize> {
            self.events
                .lock()
                .expect("progress lock")
                .iter()
                .filter_map(|event| match event {
                    ProgressEvent::TextDelta {
                        cumulative_chars, ..
                    } => Some(*cumulative_chars),
                    _ => None,
                })
                .collect()
        }
    }

    fn progress_label(event: &ProgressEvent) -> String {
        match event {
            ProgressEvent::Started { agent, max_steps } => {
                format!("started {agent} of {max_steps}")
            }
            ProgressEvent::ModelTurn { turn, of } => format!("model-turn {turn}/{of}"),
            ProgressEvent::Steered { turn } => format!("steered {turn}"),
            ProgressEvent::TextDelta {
                turn,
                text,
                cumulative_chars,
            } => format!("text-delta {turn} {:?} {cumulative_chars}", text.as_str()),
            ProgressEvent::Answered {
                turn, tool_calls, ..
            } => format!("answered {turn} calls={tool_calls}"),
            ProgressEvent::ToolStarted { word, .. } => format!("tool-started {}", word.as_str()),
            ProgressEvent::ToolFinished { word, outcome, .. } => {
                format!("tool-finished {} {outcome:?}", word.as_str())
            }
            ProgressEvent::Attachment {
                index,
                media_type,
                bytes,
            } => format!("attachment {index} {media_type} {bytes}"),
            ProgressEvent::KeepAlive { count, .. } => format!("keep-alive {count}"),
            ProgressEvent::Cancelled { by } => format!("cancelled {by:?}"),
            ProgressEvent::Failed { class } => format!("failed {class:?}"),
            ProgressEvent::Finished {
                outcome,
                turns,
                tool_calls,
                ..
            } => format!("finished {outcome:?} turns={turns} calls={tool_calls}"),
        }
    }

    struct EventingModel {
        turns: Mutex<VecDeque<AssistantTurn>>,
    }

    impl ChatModel for EventingModel {
        fn complete(
            &self,
            _messages: &[ModelMessage],
            _tools: &[ModelTool],
            _options: &CompletionOptions,
            on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            if on_event(TurnEvent::ToolCallStarted { index: 0 }).is_break() {
                return Err(InferenceError::Cancelled);
            }
            self.turns
                .lock()
                .expect("turn lock")
                .pop_front()
                .ok_or(InferenceError::Protocol(
                    dekopon_model::error::ProtocolFailure::NoChoices,
                ))
        }
    }

    struct TranscriptModel {
        events: Mutex<VecDeque<TurnEvent>>,
        turn: Mutex<Option<AssistantTurn>>,
    }

    impl TranscriptModel {
        fn new(body: &str, turn: AssistantTurn) -> Self {
            let events = dekopon_model::events_from_transcript(body)
                .expect("the recorded transcript parses");
            Self {
                events: Mutex::new(events.into()),
                turn: Mutex::new(Some(turn)),
            }
        }
    }

    impl ChatModel for TranscriptModel {
        fn complete(
            &self,
            _messages: &[ModelMessage],
            _tools: &[ModelTool],
            _options: &CompletionOptions,
            on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            let events = std::mem::take(&mut *self.events.lock().expect("event lock"));
            for event in events {
                if on_event(event).is_break() {
                    return Err(InferenceError::Cancelled);
                }
            }
            self.turn
                .lock()
                .expect("turn lock")
                .take()
                .ok_or(InferenceError::Protocol(
                    dekopon_model::error::ProtocolFailure::NoChoices,
                ))
        }
    }

    fn text_transcript(fragments: &[&str]) -> String {
        let mut body = String::new();
        for fragment in fragments {
            body.push_str(&format!(
                "data: {}\n\n",
                json!({"choices": [{"delta": {"content": fragment}}]})
            ));
        }
        body.push_str("data: [DONE]\n\n");
        body
    }

    struct StopsDuringTheTurn {
        reads: AtomicUsize,
        after: usize,
    }

    impl CancellationProbe for StopsDuringTheTurn {
        fn is_cancelled(&self) -> bool {
            self.reads.fetch_add(1, AtomicOrdering::Relaxed) >= self.after
        }

        fn cancel_source(&self) -> Option<CancelSource> {
            Some(CancelSource::User {
                via: CancelVia::NativeStop,
            })
        }
    }

    #[derive(Default)]
    struct QueuedSteers(Mutex<VecDeque<String>>);

    impl QueuedSteers {
        fn push(&self, text: &str) {
            self.0.lock().expect("steers").push_back(text.to_owned());
        }
    }

    impl SteerSource for QueuedSteers {
        fn drain(&self) -> Vec<String> {
            self.0.lock().expect("steers").drain(..).collect()
        }
    }

    enum SteerMode {
        Abort,
        Boundary,
    }

    struct SteeringModel {
        scripted: ScriptedModel,
        steers: Arc<QueuedSteers>,
        mode: SteerMode,
    }

    impl SteeringModel {
        fn new(turns: impl IntoIterator<Item = AssistantTurn>, mode: SteerMode) -> Self {
            Self {
                scripted: ScriptedModel::new(turns),
                steers: Arc::default(),
                mode,
            }
        }

        fn request(&self, index: usize) -> Vec<(String, String)> {
            self.scripted.observed_messages.lock().expect("messages")[index]
                .iter()
                .map(|message| {
                    (
                        message.role().to_owned(),
                        message.content().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        }
    }

    impl ChatModel for SteeringModel {
        fn complete(
            &self,
            messages: &[ModelMessage],
            tools: &[ModelTool],
            options: &CompletionOptions,
            on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            let first = self
                .scripted
                .observed_messages
                .lock()
                .expect("messages")
                .is_empty();
            let turn = self.scripted.complete(messages, tools, options, on_event);
            if first {
                self.steers.push("msg2");
                if matches!(self.mode, SteerMode::Abort) {
                    return Err(InferenceError::Cancelled);
                }
            }
            turn
        }
    }

    #[test]
    fn an_aborted_call_keeps_its_step_number_and_finishes_once() {
        let model = SteeringModel::new([answer("discarded"), answer("done")], SteerMode::Abort);
        let (sink, progress) = recording_sink();
        let mut history = History::default();
        let outcome = run_prompt_session(
            &model,
            &RecordingRuntime::new(0),
            SessionInputs::new("msg1", limits(1, 4))
                .with_steering(model.steers.as_ref())
                .with_progress(progress),
            &mut history,
        )
        .expect("abort does not spend the only step");
        assert_eq!(outcome.answer, "done");
        assert_eq!(outcome.model_turns, 1);
        assert_eq!(
            model.request(1),
            [
                ("user".into(), "msg1".into()),
                ("user".into(), "msg2".into())
            ]
        );
        assert_eq!(
            sink.labels(),
            [
                "model-turn 1/1",
                "steered 1",
                "model-turn 1/1",
                "answered 1 calls=0",
                "finished Answered turns=1 calls=0"
            ]
        );
        assert_eq!(history.turns()[0].user(), "msg1\n\nmsg2");
    }

    #[test]
    fn a_boundary_steer_follows_the_draft_before_the_only_finished_event() {
        let model = SteeringModel::new([answer("draft"), answer("done")], SteerMode::Boundary);
        let (sink, progress) = recording_sink();
        let mut history = History::default();
        let outcome = run_prompt_session(
            &model,
            &RecordingRuntime::new(0),
            SessionInputs::new("msg1", limits(2, 4))
                .with_steering(model.steers.as_ref())
                .with_progress(progress),
            &mut history,
        )
        .expect("the queued steer prevents the draft from finishing");
        assert_eq!(outcome.answer, "done");
        assert_eq!(outcome.model_turns, 2);
        assert_eq!(
            model.request(1),
            [
                ("user".into(), "msg1".into()),
                ("assistant".into(), "draft".into()),
                ("user".into(), "msg2".into())
            ]
        );
        assert_eq!(
            sink.labels(),
            [
                "model-turn 1/2",
                "answered 1 calls=0",
                "model-turn 2/2",
                "answered 2 calls=0",
                "finished Answered turns=2 calls=0"
            ]
        );
        assert_eq!(history.turns()[0].user(), "msg1\n\nmsg2");
        assert_eq!(history.turns()[0].answer(), Some("done"));
    }

    #[test]
    fn an_aborted_tool_proposal_never_runs_before_the_steer() {
        let model = SteeringModel::new(
            [script_call("call-1", "echo one"), answer("done")],
            SteerMode::Abort,
        );
        let runtime = RecordingRuntime::new(1);
        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("msg1", limits(2, 4)).with_steering(model.steers.as_ref()),
            &mut History::default(),
        )
        .expect("the cancelled proposal is discarded");
        assert_eq!(outcome.script_calls, 0);
        assert!(runtime.scripts.lock().expect("scripts").is_empty());
        assert_eq!(
            model.request(1),
            [
                ("user".into(), "msg1".into()),
                ("user".into(), "msg2".into())
            ]
        );
    }

    #[test]
    fn a_steer_during_a_script_follows_its_intact_result() {
        let model = ScriptedModel::new([script_call("call-1", "echo one"), answer("done")]);
        let steers = Arc::new(QueuedSteers::default());
        let mut runtime = RecordingRuntime::new(1);
        runtime.steers = Some(Arc::clone(&steers));
        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("msg1", limits(2, 4)).with_steering(steers.as_ref()),
            &mut History::default(),
        )
        .expect("script work is not interrupted");
        assert_eq!(outcome.script_calls, 1);
        assert_eq!(outcome.capability_invocations, 1);
        let messages = model.observed_messages.lock().expect("messages");
        let second = &messages[1];
        assert_eq!(
            second.iter().map(ModelMessage::role).collect::<Vec<_>>(),
            ["user", "assistant", "tool", "user"]
        );
        assert_eq!(
            second[2].content(),
            Some(
                format_script_outcome(&ScriptOutcome {
                    output: "ran 8 bytes".into(),
                    exit_code: ExitCode::SUCCESS,
                    truncated: false,
                    capability_calls: 1,
                    steps: 1,
                })
                .as_str()
            )
        );
        assert_eq!(second[3].content(), Some("msg2"));
    }

    #[test]
    fn a_stop_wins_over_a_queued_steer() {
        let model = SteeringModel::new([answer("discarded")], SteerMode::Abort);
        let probe = StopsDuringTheTurn {
            reads: AtomicUsize::new(0),
            after: 1,
        };
        let (sink, progress) = recording_sink();
        let error = run_prompt_session(
            &model,
            &RecordingRuntime::new(0),
            SessionInputs::new("msg1", limits(2, 4))
                .with_steering(model.steers.as_ref())
                .with_cancellation(&probe)
                .with_progress(progress),
            &mut History::default(),
        )
        .expect_err("a session stop is not a model interruption");
        assert!(matches!(error, PromptError::Cancelled));
        assert_eq!(model.steers.drain(), ["msg2"]);
        assert!(
            !sink
                .events
                .lock()
                .expect("events")
                .iter()
                .any(|event| matches!(
                    event,
                    ProgressEvent::Steered { .. } | ProgressEvent::Finished { .. }
                ))
        );
    }

    #[test]
    fn the_last_answer_leaves_pending_steers_for_a_follow_up() {
        let model = SteeringModel::new([answer("done")], SteerMode::Boundary);
        let mut history = History::default();
        let outcome = run_prompt_session(
            &model,
            &RecordingRuntime::new(0),
            SessionInputs::new("msg1", limits(1, 4)).with_steering(model.steers.as_ref()),
            &mut history,
        )
        .expect("the final step answers");
        assert_eq!(outcome.answer, "done");
        assert_eq!(model.steers.drain(), ["msg2"]);
        assert_eq!(history.turns()[0].user(), "msg1");
    }

    #[test]
    fn a_decline_leaves_pending_steers_for_a_follow_up() {
        let model = SteeringModel::new([decline(json!({}))], SteerMode::Boundary);
        let outcome = run_prompt_session(
            &model,
            &RecordingRuntime::new(0),
            SessionInputs::new("msg1", limits(2, 4))
                .with_steering(model.steers.as_ref())
                .with_optional_reply(),
            &mut History::default(),
        )
        .expect("decline does not drain a pending steer");
        assert_eq!(outcome.disposition, ReplyDisposition::Suppress);
        assert_eq!(model.steers.drain(), ["msg2"]);
    }

    #[test]
    fn a_two_turn_tool_session_reports_every_seam_in_order() {
        let model = ScriptedModel::new([script_call("call-1", "echo one"), answer("done")]);
        let runtime = RecordingRuntime::new(1);
        let (sink, progress) = recording_sink();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("go", limits(4, 8)).with_progress(progress),
            &mut History::default(),
        )
        .expect("the scripted session answers");

        assert_eq!(outcome.answer, "done");
        assert_eq!(
            sink.labels(),
            vec![
                "model-turn 1/4",
                "answered 1 calls=1",
                "model-turn 2/4",
                "answered 2 calls=0",
                "finished Answered turns=2 calls=1",
            ],
            "the whole session, in the order a person watching it happened"
        );
    }

    #[test]
    fn a_session_without_a_sink_answers_exactly_as_one_with_it() {
        let watched = ScriptedModel::new([script_call("call-1", "echo one"), answer("done")]);
        let unwatched = ScriptedModel::new([script_call("call-1", "echo one"), answer("done")]);
        let runtime = RecordingRuntime::new(1);
        let (_recorder, progress) = recording_sink();

        let with_sink = run_prompt_session(
            &watched,
            &runtime,
            SessionInputs::new("go", limits(4, 8)).with_progress(progress),
            &mut History::default(),
        )
        .expect("the watched session answers");
        let without_sink = run_prompt_session(
            &unwatched,
            &runtime,
            SessionInputs::new("go", limits(4, 8)),
            &mut History::default(),
        )
        .expect("the unwatched session answers");

        assert_eq!(with_sink, without_sink);
    }

    #[test]
    fn an_interrupted_turn_stops_the_session_and_names_who_asked() {
        let model = EventingModel {
            turns: Mutex::new([answer("never delivered")].into_iter().collect()),
        };
        let runtime = RecordingRuntime::new(0);
        let (sink, progress) = recording_sink();
        let probe = StopsDuringTheTurn {
            reads: AtomicUsize::new(0),
            after: 1,
        };

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("go", limits(2, 4))
                .with_cancellation(&probe)
                .with_progress(progress),
            &mut History::default(),
        )
        .expect_err("an interrupted turn ends the session");

        assert!(
            matches!(error, PromptError::Cancelled),
            "an interrupted stream is the session being stopped, not the model failing: {error}"
        );
        assert_eq!(error.telemetry_kind(), "cancelled");
        assert_eq!(
            sink.labels(),
            vec!["model-turn 1/2", "cancelled User { via: NativeStop }",],
            "no answer was ever reported for the turn that was cut off"
        );
    }

    #[test]
    fn every_streamed_fragment_reaches_the_surface_with_its_running_character_count() {
        let model = TranscriptModel::new(
            &text_transcript(&["Hel", "lo there"]),
            answer("Hello there"),
        );
        let runtime = RecordingRuntime::new(0);
        let (sink, progress) = recording_sink();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("go", limits(2, 4)).with_progress(progress),
            &mut History::default(),
        )
        .expect("a streamed turn answers like any other");

        assert_eq!(outcome.answer, "Hello there");
        assert_eq!(
            sink.labels(),
            vec![
                "model-turn 1/2",
                "text-delta 1 \"Hel\" 3",
                "text-delta 1 \"lo there\" 11",
                "answered 1 calls=0",
                "finished Answered turns=1 calls=0",
            ],
            "each fragment as it arrived, with the turn's running count"
        );
    }

    #[test]
    fn fragments_stop_at_the_outbound_bound_and_the_turn_still_answers() {
        let half = STREAMED_TEXT_BOUND_BYTES / 2;
        let fragment = "x".repeat(half);
        let model = TranscriptModel::new(
            &text_transcript(&[&fragment, &fragment, &fragment]),
            answer("delivered whole"),
        );
        let runtime = RecordingRuntime::new(0);
        let (sink, progress) = recording_sink();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("go", limits(2, 4)).with_progress(progress),
            &mut History::default(),
        )
        .expect("passing the bound is not a failure");

        assert_eq!(
            sink.forwarded_chars(),
            vec![half, half * 2],
            "the fragment that would carry the text past the bound is not forwarded"
        );
        assert_eq!(
            outcome.answer, "delivered whole",
            "the answer is the turn's, not what the surface was shown"
        );
    }

    #[test]
    fn a_session_that_runs_out_of_turns_reports_the_budget_it_spent() {
        let model = ScriptedModel::new([script_call("call-1", "echo one")]);
        let runtime = RecordingRuntime::new(1);
        let (sink, progress) = recording_sink();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("go", limits(1, 4)).with_progress(progress),
            &mut History::default(),
        )
        .expect_err("one turn that asks for a tool cannot answer");

        assert!(
            matches!(error, PromptError::MaxSteps { maximum: 1 }),
            "{error}"
        );
        assert_eq!(
            sink.labels(),
            vec!["model-turn 1/1", "answered 1 calls=1", "failed StepBudget",]
        );
    }

    #[test]
    fn a_zero_step_session_still_tells_a_waiting_surface_that_it_ended() {
        let model = ScriptedModel::new([answer("unreachable")]);
        let runtime = RecordingRuntime::new(0);
        let (sink, progress) = recording_sink();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("go", limits(0, 4)).with_progress(progress),
            &mut History::default(),
        )
        .expect_err("a zero-step session is refused");

        assert!(matches!(error, PromptError::ZeroSteps), "{error}");
        assert_eq!(sink.labels(), vec!["failed Internal"]);
    }

    #[test]
    fn a_declined_continuation_finishes_with_nothing_to_deliver() {
        let model = ScriptedModel::new([decline(json!({}))]);
        let runtime = RecordingRuntime::new(0);
        let (sink, progress) = recording_sink();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("go", limits(2, 4))
                .with_optional_reply()
                .with_progress(progress),
            &mut History::default(),
        )
        .expect("a decline is a completed session");

        assert_eq!(outcome.disposition, ReplyDisposition::Suppress);
        assert_eq!(
            sink.labels(),
            vec![
                "model-turn 1/2",
                "answered 1 calls=1",
                "finished Declined turns=1 calls=1",
            ]
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
                memory: MemoryConfigView::OneShot,
            },
            vec![EffectiveCapabilityView {
                id: "gh.pull-request.read".to_owned(),
                provider: "gh".to_owned(),
                description: "Reads one pull request".to_owned(),
                effect: "read-only".to_owned(),
                risk: "Low".to_owned(),
            }],
        )
    }

    fn agent_config_tool_call(id: &str, arguments: Value) -> ModelToolCall {
        ModelToolCall {
            id: id.into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: AGENT_CONFIG_TOOL_NAME.to_owned(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn agent_config_call(arguments: Value) -> AssistantTurn {
        AssistantTurn::new(
            None,
            vec![agent_config_tool_call("config-call", arguments)],
            None,
        )
    }

    fn conversation(count: usize) -> Vec<ConversationTurn> {
        (1..=count)
            .map(|index| {
                ConversationTurn::completed(format!("ask {index}"), format!("answer {index}"))
            })
            .collect()
    }

    #[derive(Default)]
    struct UsageRecorder(Mutex<Vec<Option<ModelUsage>>>);

    impl ModelUsageObserver for UsageRecorder {
        fn observe(&self, usage: Option<ModelUsage>) {
            self.0.lock().expect("usage observations lock").push(usage);
        }
    }

    fn assert_window_is_well_formed(history: &History) {
        let mut messages = Vec::new();
        history.replay_into(&mut messages);

        let expected = history
            .turns()
            .iter()
            .map(|turn| 1 + usize::from(turn.is_answered()))
            .sum::<usize>();
        assert_eq!(messages.len(), expected);

        for (index, message) in messages.iter().enumerate() {
            let encoded = serde_json::to_value(message).expect("a message serializes");
            let fields = encoded.as_object().expect("a message is a JSON object");
            assert!(
                matches!(message.role(), "user" | "assistant"),
                "message {index} replays role {:?}",
                message.role()
            );
            assert!(
                !fields.contains_key("tool_calls"),
                "message {index} replays a tool call nothing answers"
            );
            assert!(
                !fields.contains_key("tool_call_id"),
                "message {index} replays an orphaned tool result"
            );
            // A replayed message must carry plain text content, or the ChatGPT backend silently
            // drops it with no error.
            assert!(
                fields.contains_key("content"),
                "message {index} replays without content"
            );
        }

        let mut position = 0;
        for turn in history.turns() {
            assert_eq!(messages[position].role(), "user");
            assert_eq!(messages[position].content(), Some(turn.user()));
            position += 1;
            if let Some(answer) = turn.answer() {
                assert_eq!(messages[position].role(), "assistant");
                assert_eq!(messages[position].content(), Some(answer));
                position += 1;
            }
        }
    }

    #[test]
    fn token_observer_sees_reported_and_unreported_successful_model_responses() {
        let mut first = script_call("call-1", "echo hello");
        let expected = ModelUsage {
            input_tokens: Some(41),
            output_tokens: Some(5),
            ..ModelUsage::default()
        };
        first.usage = Some(expected);
        let model = ScriptedModel::new([first, answer("done")]);
        let runtime = RecordingRuntime::new(0);
        let observer = UsageRecorder::default();
        let mut history = History::default();

        run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("run it", limits(3, 4)).with_usage_observer(&observer),
            &mut history,
        )
        .expect("session succeeds");

        assert_eq!(
            *observer.0.lock().expect("usage observations lock"),
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
                ("user", "ask 2".to_owned()),
                ("assistant", "answer 2".to_owned()),
                ("user", "and now?".to_owned()),
            ]
        );
    }

    #[test]
    fn the_instructions_are_prepended_once_per_call_and_never_remembered() {
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
    fn a_remembered_exchange_carries_no_tool_traffic() {
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
            next.first_roles(),
            vec![
                ("user", "do the work".to_owned()),
                ("assistant", "I ran two scripts.".to_owned()),
                ("user", "and again".to_owned()),
            ]
        );
        assert!(next.tool_messages().is_empty());
    }

    #[test]
    fn every_cut_point_leaves_whole_exchanges() {
        let turns = conversation(6);
        let total_bytes = turns.iter().map(ConversationTurn::bytes).sum::<usize>();

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

        history.record(ConversationTurn::completed(
            "a long question",
            "a long answer",
        ));

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
                ("user", "ask 4".to_owned()),
                ("assistant", "answer 4".to_owned()),
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
            retry.first_roles(),
            vec![
                ("user", "loop forever".to_owned()),
                ("user", "try again".to_owned()),
            ]
        );
        assert!(retry.tool_messages().is_empty());
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

    struct OptionsObserver {
        turns: Mutex<VecDeque<AssistantTurn>>,
        observed: Mutex<Vec<Option<String>>>,
    }

    impl ChatModel for OptionsObserver {
        fn complete(
            &self,
            _messages: &[ModelMessage],
            _tools: &[ModelTool],
            options: &CompletionOptions,
            _on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        ) -> Result<AssistantTurn, InferenceError> {
            self.observed
                .lock()
                .expect("options lock")
                .push(options.prompt_cache_key().map(str::to_owned));
            self.turns
                .lock()
                .expect("turn lock")
                .pop_front()
                .ok_or(InferenceError::Protocol(
                    dekopon_model::error::ProtocolFailure::NoChoices,
                ))
        }
    }

    #[test]
    fn every_turn_of_a_session_carries_the_same_routing_metadata() {
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

    #[test]
    fn provider_command_words_are_offered_to_the_model() {
        let tool = script_tool(&["gh".to_owned(), "fly".to_owned(), "gh".to_owned()]);
        assert!(
            tool.description.contains("command words: fly, gh."),
            "{}",
            tool.description
        );
        assert_eq!(
            tool.description.matches("run `<word> --help`").count(),
            1,
            "{}",
            tool.description
        );
        assert_no_doubled_spaces(&tool.description);
    }

    fn assert_no_doubled_spaces(description: &str) {
        assert!(
            !description.contains("  "),
            "the tool description contains a run of spaces: {description}"
        );
    }

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

    #[test]
    fn every_construct_the_description_calls_an_error_is_refused_by_the_shell() {
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

    struct OutcomeCapabilities;

    impl CapabilityInvoker for OutcomeCapabilities {
        fn granted(&self) -> Vec<String> {
            ["posts.get", "locked.door", "broken.thing"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        }

        fn command_words(&self) -> Vec<String> {
            vec!["probe".to_owned()]
        }

        fn run_command(
            &self,
            word: &str,
            argv: &[String],
            _stdin: Option<&str>,
        ) -> Option<CommandRun> {
            if word != "probe" {
                return None;
            }
            let proposed = |capability: &str, input: Value| CommandRun::Proposed {
                capability: capability.to_owned(),
                input,
                secret_use: None,
            };
            let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
            Some(match argv.as_slice() {
                ["--help"] => CommandRun::Rendered {
                    stdout: "Usage: probe <get|door|broken|vault>\n".to_owned(),
                    stderr: String::new(),
                    status: 0,
                },
                ["get", "--post-id", id] => proposed("posts.get", json!({ "postId": id })),
                ["door"] => proposed("locked.door", json!({})),
                ["broken"] => proposed("broken.thing", json!({})),
                ["vault"] => proposed("vault.open", json!({})),
                _ => CommandRun::Failed {
                    message: "probe: usage: probe <get|door|broken|vault>".to_owned(),
                },
            })
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
                    detail: None,
                },
                _ => CapabilityCallResult::NotFound,
            }
        }
    }

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
                "Exit 127 means the word is not a builtin or a command this session's providers add",
            ),
        ] {
            assert!(SCRIPT_TOOL_DESCRIPTION.contains(phrase), "{phrase}");
            assert!(
                phrase.contains(&format!("Exit {} ", code.get())),
                "{phrase} must name exit code {}",
                code.get()
            );
        }

        let not_found = dekopon_shell::run("wikipedia_page --title x", &OutcomeCapabilities);
        assert_eq!(not_found.exit_code, ExitCode::NOT_FOUND, "{not_found:?}");
        assert!(
            not_found.output.contains("command not found"),
            "{not_found:?}"
        );

        let ungranted = dekopon_shell::run("probe vault", &OutcomeCapabilities);
        assert_eq!(ungranted.exit_code, ExitCode::NOT_FOUND, "{ungranted:?}");
        assert!(ungranted.output.contains("vault.open"), "{ungranted:?}");

        let denied = dekopon_shell::run("probe door", &OutcomeCapabilities);
        assert_eq!(denied.exit_code, ExitCode::DENIED, "{denied:?}");

        let failed = dekopon_shell::run("probe broken", &OutcomeCapabilities);
        assert_eq!(failed.exit_code, ExitCode::FAILURE, "{failed:?}");
        assert!(
            failed
                .output
                .contains("broken.thing: failed: upstream boom"),
            "{failed:?}"
        );

        let usage = dekopon_shell::run("echo abc | grep '[0-9]'", &OutcomeCapabilities);
        assert_eq!(usage.exit_code, ExitCode::SYNTAX, "{usage:?}");
        let declined = dekopon_shell::run("probe bogus", &OutcomeCapabilities);
        assert_eq!(declined.exit_code, ExitCode::SYNTAX, "{declined:?}");
        assert!(declined.output.contains("probe: usage:"), "{declined:?}");

        let help = dekopon_shell::run("probe --help", &OutcomeCapabilities);
        assert_eq!(help.exit_code, ExitCode::SUCCESS, "{help:?}");
        assert!(help.output.contains("Usage: probe"), "{help:?}");
        let proposed = dekopon_shell::run("probe get --post-id 7", &OutcomeCapabilities);
        assert_eq!(proposed.exit_code, ExitCode::SUCCESS, "{proposed:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&proposed.output)
                .expect("the proposal's input is echoed as JSON"),
            json!({"postId": "7"})
        );
        let listed = dekopon_shell::run("cap --list", &OutcomeCapabilities);
        assert_eq!(listed.exit_code, ExitCode::SUCCESS, "{listed:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&listed.output).expect("cap --list prints a JSON array"),
            json!(["broken.thing", "locked.door", "posts.get"])
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
        assert!(tool.description.contains("cap --list"));
        assert_eq!(
            tool.description.matches("run `<word> --help`").count(),
            1,
            "{}",
            tool.description
        );
        for retired in [
            "kebab",
            "JSON object",
            "capability invocation",
            "cap --describe",
            "curl",
        ] {
            assert!(
                !tool.description.contains(retired),
                "{retired}: {}",
                tool.description
            );
        }
        assert!(
            !tool
                .description
                .contains("providers add these command words")
        );
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
            SessionInputs::new("what is your configuration?", limits(4, 32))
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
            AssistantTurn::new(
                None,
                vec![
                    agent_config_tool_call("config-call-1", json!({})),
                    agent_config_tool_call("config-call-2", json!({})),
                ],
                None,
            ),
            answer("done"),
        ]);
        let runtime = RecordingRuntime::new(0);
        let config = agent_config();
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("inspect twice", limits(3, 32)).with_agent_config(&config),
            &mut history,
        )
        .expect("repeated inspection succeeds");

        assert_eq!(outcome.script_calls, 0);
        assert_eq!(outcome.capability_invocations, 0);
        assert!(runtime.scripts.lock().expect("script lock").is_empty());

        let messages = model.tool_messages();
        assert_eq!(messages.len(), 2);
        let first: Value =
            serde_json::from_str(&messages[0]).expect("the first configuration is JSON");
        assert_eq!(first["agent"]["id"], "reviewer");
        assert!(first.get("error").is_none());
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
            SessionInputs::new("inspect on two turns", limits(4, 32)).with_agent_config(&config),
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
            data: dekopon_model::asset::DiskBlob::from_bytes(text.as_bytes())
                .expect("spool")
                .into(),
        }
    }

    fn asset_call(id: u64) -> AssistantTurn {
        AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: "asset-call".into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: ASSET_TOOL_NAME.to_owned(),
                    arguments: json!({ "id": id }).to_string(),
                },
            }],
            None,
        )
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
            SessionInputs::new("what is in the file?", limits(3, 32)).with_assets(&assets),
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
        let text = "☃".repeat(MAX_TEXTUAL_ASSET_BYTES);
        let model = ScriptedModel::new([asset_call(1), answer("The file was too large to read.")]);
        let runtime = RecordingRuntime::new(0);
        let assets = FixedAssets(vec![text_asset(&text)]);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("what is in the file?", limits(3, 32)).with_assets(&assets),
            &mut history,
        )
        .expect("an oversized asset is an outcome, not a failed session");

        assert_eq!(outcome.answer, "The file was too large to read.");
        let messages = model.tool_messages();
        assert_eq!(messages.len(), 1);
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
            SessionInputs::new("inspect", limits(1, 32)).with_agent_config(&config),
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
            SessionInputs::new("OK, thanks", limits(2, 4)).with_optional_reply(),
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
            SessionInputs::new("thanks", limits(2, 4)),
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
        let model = ScriptedModel::new([AssistantTurn::new(
            None,
            vec![
                decline_call("decline-call", json!({})),
                ModelToolCall {
                    id: "script-call".into(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: SCRIPT_TOOL_NAME.to_owned(),
                        arguments: json!({"script": "echo should-not-run"}).to_string(),
                    },
                },
            ],
            None,
        )]);
        let runtime = RecordingRuntime::new(1);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("conversation moved on", limits(2, 4)).with_optional_reply(),
            &mut history,
        )
        .expect("the no-reply decision is terminal");

        assert_eq!(outcome.disposition, ReplyDisposition::Suppress);
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
        assert_eq!(outcome.capability_invocations, 0);
        let tools = model.observed_tools.lock().expect("tool observations lock");
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
            SessionInputs::new("maybe do this", limits(4, 4)).with_optional_reply(),
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
            SessionInputs::new("maybe do this", limits(2, 4)).with_optional_reply(),
            &mut history,
        )
        .expect_err("work cannot disappear behind a final-turn decline");

        assert!(matches!(error, PromptError::UnreportedCapabilityWork));
        assert_eq!(
            history.turns().last().and_then(ConversationTurn::answer),
            None
        );
    }

    #[test]
    fn the_decline_tool_rejects_model_supplied_fields() {
        let model = ScriptedModel::new([decline(json!({"message": "secret"}))]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let error = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("optional", limits(1, 4)).with_optional_reply(),
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
            script_call("call-1", "probe upper --text hi | jq -r .text"),
            answer("The probe answered HI."),
        ]);
        let runtime = RecordingRuntime::new(1);

        let outcome = run_prompt(&model, &runtime, "say hi", None, limits(4, 32))
            .expect("prompt session succeeds");

        assert_eq!(outcome.answer, "The probe answered HI.");
        assert_eq!(outcome.model_turns, 2);
        assert_eq!(outcome.script_calls, 1);
        assert_eq!(outcome.capability_invocations, 1);
        let scripts = runtime.scripts.lock().expect("script lock");
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].0, "probe upper --text hi | jq -r .text");
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
        let model = ScriptedModel::new([AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: "call-1".into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: "echo_echo".to_owned(),
                    arguments: "{}".to_owned(),
                },
            }],
            None,
        )]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "call the old tool", None, limits(1, 32))
            .expect_err("unknown tools must fail closed");

        assert!(matches!(error, PromptError::UnknownTool(_)));
        assert!(runtime.scripts.lock().expect("script lock").is_empty());
    }

    #[test]
    fn rejects_tool_calls_without_a_string_script_argument() {
        for arguments in [r#"{"command":"echo hi"}"#, r#"{"script":42}"#, "{}"] {
            let model = ScriptedModel::new([AssistantTurn::new(
                None,
                vec![ModelToolCall {
                    id: "call-1".into(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: SCRIPT_TOOL_NAME.to_owned(),
                        arguments: arguments.to_owned(),
                    },
                }],
                None,
            )]);
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
                id: format!("call-{index}").into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            })
            .collect();
        let model =
            ScriptedModel::new([AssistantTurn::new(None, tool_calls, None), answer("done")]);
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
                id: format!("call-{index}").into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            })
            .collect();
        let model = ScriptedModel::new([AssistantTurn::new(None, tool_calls, None)]);
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

    struct BlockingBridgeRuntime {
        handle: tokio::runtime::Handle,
        dispatched: Arc<Mutex<Vec<String>>>,
    }

    impl ScriptRuntime for BlockingBridgeRuntime {
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
                script_call("call-1", "httpprobe fetch --uri https://example.test"),
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
            vec!["httpprobe fetch --uri https://example.test".to_owned()]
        );
    }

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
        let model = ScriptedModel::new([AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: "  ".into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: json!({ "script": "echo hi" }).to_string(),
                },
            }],
            None,
        )]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "correlate", None, limits(1, 32))
            .expect_err("an uncorrelated tool call must fail closed");

        assert!(matches!(error, PromptError::EmptyToolCallId));
    }

    #[test]
    fn rejects_arguments_that_are_not_a_json_object() {
        let model = ScriptedModel::new([AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: "call-1".into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: SCRIPT_TOOL_NAME.to_owned(),
                    arguments: Value::String("echo hi".to_owned()).to_string(),
                },
            }],
            None,
        )]);
        let runtime = RecordingRuntime::new(0);

        let error = run_prompt(&model, &runtime, "malformed", None, limits(1, 32))
            .expect_err("non-object arguments must fail closed");

        assert!(matches!(error, PromptError::ArgumentsNotObject { .. }));
    }

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
        AssistantTurn::new(
            None,
            vec![ModelToolCall {
                id: id.into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: name.to_owned(),
                    arguments: arguments.to_string(),
                },
            }],
            None,
        )
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
            SessionInputs::new("review PR 7", limits(5, 2))
                .with_system(Some("Be concise."))
                .with_skills(&skills),
            &mut history,
        )
        .expect("skill reads are recoverable model turns");

        assert_eq!(outcome.answer, "Reviewed.");
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
            SessionInputs::new("review", limits(4, 2)).with_skills(&skills),
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
            SessionInputs::new("review", limits(2, 2)).with_system(Some("Be concise.")),
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
            SessionInputs::new("review", limits(2, 2)).with_skills(&skills),
            &mut history,
        )
        .expect_err("an unexpected field is malformed model output");

        assert!(matches!(
            error,
            PromptError::UnexpectedSkillArguments { .. }
        ));
    }

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
            suggestion("s-4", "gh.issue.comment"),
            answer("Done."),
        ]);
        let runtime = RecordingRuntime::new(0);
        let mut history = History::default();

        let outcome = run_prompt_session(
            &model,
            &runtime,
            SessionInputs::new("do the thing", limits(6, 2)).with_improvement_suggestions(),
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
            SessionInputs::new("do the thing", limits(3, 2)).with_improvement_suggestions(),
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
}
