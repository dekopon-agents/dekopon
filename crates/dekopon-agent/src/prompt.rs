//! The model tool loop, exposing one sandboxed scripting tool rather than one tool per capability.
//!
//! `dekopon-shell` is the interpreter; this module is the model-facing half. Every session offers
//! [`SCRIPT_TOOL_NAME`], whose single argument is a script. An embedding gateway may additionally
//! offer credential-free agent configuration and chat-asset tools. Provider work still happens
//! only inside the script instead of across many small capability-shaped model tools.

use std::{fmt, time::Instant};

use dekopon_model::model::{
    ChatModel, CompletionOptions, ContentPart, ModelError, ModelMessage, ModelTool, ModelToolCall,
    ModelUsage, assistant_message,
};
use dekopon_shell::ScriptOutcome;
use serde_json::{Value, json};
use thiserror::Error;

use crate::{meta::AgentConfigView, milliseconds};

mod history;

pub use history::{ConversationTurn, DEFAULT_MAX_BYTES, DEFAULT_MAX_TURNS, History, HistoryLimits};

/// Model-facing name of the single scripting tool.
///
/// Named for what it resembles rather than what it is. Models have overwhelming priors about a
/// tool called `bash`, and almost all of them transfer: pipelines, `&&`, `$( )`, exit codes. The
/// description below spends its length on the places where those priors are wrong.
pub const SCRIPT_TOOL_NAME: &str = "bash";

/// The tool a model calls to inspect this session's credential-free agent configuration.
pub const AGENT_CONFIG_TOOL_NAME: &str = "inspect_agent_config";

/// The tool a model calls to look at something a person attached to their message.
pub const ASSET_TOOL_NAME: &str = "fetch_chat_asset";

/// Tool calls a single model turn may request.
///
/// This bound used to cover one capability invocation each, so 32 was a statement about how much
/// provider work one turn could drive. With one scripting tool it no longer is: a single script
/// can drive many invocations, so the real work bound moved to
/// [`PromptLimits::max_capability_calls`], which the interpreter enforces per script and this loop
/// enforces across the session.
///
/// What is left is a well-formedness bound. A scripting tool expresses a multi-step plan *inside*
/// one script, while embedder-owned meta tools can legitimately fan out over a bounded attachment
/// set. Ten calls leave room for that parallel work; anything beyond ten is a runaway rather than
/// a plan.
const MAX_TOOL_CALLS_PER_TURN: usize = 10;

/// Text one chat asset may contribute to the prompt.
///
/// A textual asset arrives as a tool result, and the other tool result a session produces — a
/// script's combined output — is already capped at this exact ceiling by the interpreter. A
/// gateway's own asset budget is sized for images on the wire (8 MiB), which as `text/plain` is
/// roughly two million tokens: handing that to a provider ends the session with a context-length
/// rejection instead of an answer, which is precisely what the asset design refuses to do.
const MAX_TEXTUAL_ASSET_BYTES: usize = dekopon_shell::DEFAULT_MAX_OUTPUT_BYTES;

/// Script execution boundary consumed by the prompt loop.
///
/// This deliberately returns no `Result`. A script failure — a parse error, an exhausted budget, a
/// capability that policy refused — is a script *outcome*, and the model reads it and recovers the
/// same way it would from a non-zero exit code in a terminal. Only a broken session aborts the
/// loop.
pub trait ScriptRuntime {
    /// Runs one model-authored script, invoking at most `max_capability_calls` capabilities.
    ///
    /// The ceiling is supplied per call rather than fixed at construction because the prompt loop
    /// spends one session-wide budget across every script it runs.
    fn run_script(&self, script: &str, max_capability_calls: u32) -> ScriptOutcome;

    /// Returns the command words loaded providers contribute to this session.
    ///
    /// Defaulted to none so an embedder with no providers, and every existing implementor, is
    /// unaffected. What comes back is already filtered to providers the session holds a grant on,
    /// so a principal granted nothing is never told a word exists.
    fn command_words(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One attachment, fetched.
#[derive(Clone, Eq, PartialEq)]
pub struct FetchedAsset {
    /// The name the sender gave it.
    pub name: String,
    /// IANA media type.
    pub mime: String,
    /// The bytes themselves.
    pub data: Vec<u8>,
}

impl fmt::Debug for FetchedAsset {
    /// Summarised rather than printed, for the same reason [`ContentPart`] is.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FetchedAsset")
            .field("name", &self.name)
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// The attachments one conversation can show a model.
///
/// Deliberately pull rather than push. A screenshot costs tokens on every turn it appears in, and
/// most turns do not need to look at it — so the prompt carries a one-line reference and the model
/// spends the bytes only when it decides the answer depends on them.
///
/// Every refusal is a `String` the model reads, never an error that ends the session: an asset that
/// is too large, expired, or simply not there is something a model can work around by saying so,
/// and killing a session over it would turn a recoverable answer into silence. The implementation
/// owns its own budget for the same reason the shell runtime owns its capability budget.
pub trait AssetSource {
    /// Returns one asset's bytes, or a reason the model can read.
    fn fetch(&self, id: u64) -> Result<FetchedAsset, String>;

    /// Whether this conversation has any attachments at all.
    ///
    /// The tool is not offered when it answers `true`, because a tool that can only fail is a tool
    /// a model will still try.
    fn is_empty(&self) -> bool;
}

/// Observes provider-reported token accounting without influencing a model session.
///
/// The observer receives one call after every successfully decoded model response, including a
/// response whose provider omitted usage and a response followed by a later tool/session failure.
/// It is operational accounting only and must never be used to authorize or alter the session.
pub trait ModelUsageObserver: Send + Sync {
    /// Records the provider's report, or `None` when it reported no token counts.
    fn observe(&self, usage: Option<ModelUsage>);
}

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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptLimits {
    /// Maximum model turns, including the turn that produces the final answer.
    pub max_steps: u32,
    /// Capability invocations the whole session may drive, summed across every script.
    pub max_capability_calls: u32,
}

/// Result of a completed prompt/tool session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptOutcome {
    /// Final assistant text.
    pub answer: String,
    /// Number of model requests made.
    pub model_turns: u32,
    /// Number of scripts the model ran.
    pub script_calls: u32,
    /// Capability invocations those scripts drove.
    pub capability_invocations: u32,
}

/// Runs a bounded prompt/tool loop over one scripting tool.
///
/// This is synchronous on purpose. Both boundaries it sits between — `ChatModel` and
/// [`ScriptRuntime`] — are synchronous by design, so the caller runs the whole loop on a blocking
/// task rather than colouring these signatures `async`.
///
/// The session starts from an empty conversation and forgets it on the way out, which is what a
/// one-shot invocation wants. Use [`run_prompt_with_history`] to continue a conversation across
/// calls.
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

/// Runs one bounded prompt/tool session as the continuation of an earlier conversation.
///
/// `history` is both the input and the output: the remembered exchanges are replayed ahead of
/// `prompt`, and this session's own exchange is recorded into it before returning. It is an
/// accumulator rather than a returned value on purpose. A session that fails still consumed the
/// operator's message, and a signature returning `(PromptOutcome, History)` hands the history back
/// only on the success path — every caller writing the natural `?` would silently drop the
/// conversation exactly when a turn had gone wrong and the operator was about to retry. Borrowing
/// the accumulator makes losing it impossible: whatever the caller does with the `Result`, the
/// exchange is already recorded. See [`ConversationTurn::unanswered`] for what a failed turn
/// leaves behind.
///
/// `system` is supplied fresh on every call and is never remembered; [`ConversationTurn`] explains
/// the request corruption that separation prevents. The upside is that editing an agent's
/// instructions takes effect on the next message without rewriting a single stored conversation.
/// The matching obligation is on the caller: pass the *same* `system` for every call of one
/// conversation unless a change is intended. Instructions are hoisted out of the message list
/// entirely on the ChatGPT path, so changing them — including changing between `None` and
/// `Some`, since an absent system prompt is replaced by that backend's own default rather than by
/// nothing — rewrites the front of every subsequent request and discards the provider's prompt
/// cache for the conversation.
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

/// Everything one bounded session needs beyond the model and the script runtime.
///
/// A builder rather than more parameters: the entry point above already carries seven, and each
/// capability a session gains would otherwise add both a parameter and a longer function name to
/// every caller that does not want it. Fields are private so a later addition stays additive.
pub struct SessionInputs<'a> {
    prompt: &'a str,
    system: Option<&'a str>,
    limits: PromptLimits,
    options: Option<&'a CompletionOptions>,
    assets: Option<&'a dyn AssetSource>,
    usage_observer: Option<&'a dyn ModelUsageObserver>,
    agent_config: Option<&'a AgentConfigView>,
    cancellation: Option<&'a dyn CancellationProbe>,
}

impl<'a> SessionInputs<'a> {
    /// The two things every session has: what was asked, and what it may spend answering.
    #[must_use]
    pub const fn new(prompt: &'a str, limits: PromptLimits) -> Self {
        Self {
            prompt,
            system: None,
            limits,
            options: None,
            assets: None,
            usage_observer: None,
            agent_config: None,
            cancellation: None,
        }
    }

    /// Standing instructions, supplied fresh per call and never remembered.
    #[must_use]
    pub const fn with_system(mut self, system: Option<&'a str>) -> Self {
        self.system = system;
        self
    }

    /// Per-request model options, such as a prompt cache key.
    #[must_use]
    pub const fn with_options(mut self, options: &'a CompletionOptions) -> Self {
        self.options = Some(options);
        self
    }

    /// The attachments this conversation can show the model.
    #[must_use]
    pub const fn with_assets(mut self, assets: &'a dyn AssetSource) -> Self {
        self.assets = Some(assets);
        self
    }

    /// Adds an informational observer for provider-reported token accounting.
    #[must_use]
    pub const fn with_usage_observer(mut self, observer: &'a dyn ModelUsageObserver) -> Self {
        self.usage_observer = Some(observer);
        self
    }

    /// Adds the credential-free, subject-specific agent configuration meta tool.
    #[must_use]
    pub const fn with_agent_config(mut self, config: &'a AgentConfigView) -> Self {
        self.agent_config = Some(config);
        self
    }

    /// Adds a request-scoped cooperative cancellation probe.
    #[must_use]
    pub const fn with_cancellation(mut self, cancellation: &'a dyn CancellationProbe) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

/// Optional, request-scoped surfaces handed to the inner model loop.
#[derive(Clone, Copy)]
struct SessionExtensions<'a> {
    options: &'a CompletionOptions,
    assets: Option<&'a dyn AssetSource>,
    usage_observer: Option<&'a dyn ModelUsageObserver>,
    agent_config: Option<&'a AgentConfigView>,
    cancellation: Option<&'a dyn CancellationProbe>,
}

/// Runs one bounded prompt/tool session from a [`SessionInputs`].
///
/// The general form of [`run_prompt_with_history_and_options`], which is this function with the
/// defaults filled in.
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
        usage_observer,
        agent_config,
        cancellation,
    } = inputs;
    let fallback = CompletionOptions::default();
    let options = options.unwrap_or(&fallback);
    if limits.max_steps == 0 {
        // Nothing is recorded here: a zero-step session builds no request, so the prompt never
        // reached a model and the conversation must not claim otherwise.
        return Err(PromptError::ZeroSteps);
    }

    // Order matters and is fixed here rather than left to callers: instructions first, then what
    // the conversation remembers, then what the operator just said.
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(ModelMessage::system(system));
    }
    history.replay_into(&mut messages);
    messages.push(ModelMessage::user(prompt));

    let result = run_session(
        model,
        runtime,
        messages,
        limits,
        SessionExtensions {
            options,
            assets,
            usage_observer,
            agent_config,
            cancellation,
        },
    );
    history.record(match &result {
        Ok(outcome) => ConversationTurn::completed(prompt, outcome.answer.as_str()),
        Err(_) => ConversationTurn::unanswered(prompt),
    });
    result
}

/// Drives the model turns for one session over an already-seeded message vector.
///
/// Split out so that every exit path — answer, budget exhaustion, refused tool call, transport
/// failure — funnels back through one caller that records the exchange.
fn run_session<M, R>(
    model: &M,
    runtime: &R,
    mut messages: Vec<ModelMessage>,
    limits: PromptLimits,
    extensions: SessionExtensions<'_>,
) -> Result<PromptOutcome, PromptError>
where
    M: ChatModel + ?Sized,
    R: ScriptRuntime + ?Sized,
{
    let SessionExtensions {
        options,
        assets,
        usage_observer,
        agent_config,
        cancellation,
    } = extensions;
    // Offered only when this conversation actually carries something. A tool that can only fail is
    // a tool a model will still call, and every unusable tool costs prompt tokens on every turn.
    let assets = assets.filter(|source| !source.is_empty());
    let mut model_tools = vec![script_tool(&runtime.command_words())];
    if agent_config.is_some() {
        model_tools.push(agent_config_tool());
    }
    if assets.is_some() {
        model_tools.push(asset_tool());
    }

    let session_span = tracing::info_span!(
        "prompt.session",
        prompt.max_steps = limits.max_steps,
        prompt.max_capability_calls = limits.max_capability_calls
    );
    let _session = session_span.enter();
    let mut script_calls = 0_u32;
    let mut capability_invocations = 0_u32;
    // How much of the message vector the transcript log has already shipped, so later turns log
    // what was appended rather than the whole conversation again.
    let mut transcribed = 0_usize;
    // One full configuration copy per session. Every later call points at it instead of appending
    // a second, because a tool result stays in the message vector and is re-sent on every turn.
    let mut agent_config_shown = false;

    for model_turns in 1..=limits.max_steps {
        check_cancelled(cancellation)?;
        // Usage fields are declared empty and recorded once the provider answers: token counts
        // are response data, and they belong on the turn span so a trace query can price a
        // session without leaving the trace.
        let model_span = tracing::info_span!(
            "prompt.model_turn",
            model.turn = model_turns,
            usage.input_tokens = tracing::field::Empty,
            usage.cached_input_tokens = tracing::field::Empty,
            usage.output_tokens = tracing::field::Empty,
            usage.reasoning_output_tokens = tracing::field::Empty,
            usage.total_tokens = tracing::field::Empty,
        );
        let model_entered = model_span.enter();
        // Verbatim transcript rides the log stream rather than span attributes: a conversation is
        // unbounded text, span attributes are the wrong container for it, and the log stream is
        // what a backend indexes for full-text search. Both carry the same trace and span IDs, so
        // a log result still pivots to the turn it belongs to.
        //
        // Only the first turn ships the whole thing. Turn N's message vector strictly contains
        // turn N-1's, so re-shipping it every turn would cost a session O(N^2) payload bytes to
        // repeat what this turn's `agent.model.answer`, `agent.tool.script`, and
        // `agent.tool.output` already said. Later turns log the messages appended since the
        // previous one, so the events of a session still concatenate back into the exact request.
        if dekopon_core::telemetry_payloads() {
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
        }
        let model_started = Instant::now();
        let turn = match model.complete_with(&messages, &model_tools, options) {
            Ok(turn) => turn,
            Err(error) => {
                tracing::error!(
                    target: "dekopon_agent::audit",
                    {
                        audit.event = "accounting.model.turn",
                        model.turn = model_turns,
                        duration_ms = milliseconds(model_started.elapsed()),
                        outcome = "failed",
                    },
                    "model turn failed"
                );
                return Err(error.into());
            }
        };
        if let Some(observer) = usage_observer {
            observer.observe(turn.usage);
        }
        if let Some(usage) = &turn.usage {
            record_usage(&model_span, usage);
        }
        // Accounting rather than lifecycle: the `prompt.model_turn` span already says a turn
        // happened and how long it took. This record exists to outlive trace retention and survive
        // sampling, because a model turn is a billed call and "how many did we make, at what
        // latency, for how many tokens" is a question asked long after the trace is gone.
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
        if dekopon_core::telemetry_payloads() {
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
        }
        drop(model_entered);
        check_cancelled(cancellation)?;
        messages.push(assistant_message(&turn));

        if turn.tool_calls.is_empty() {
            check_cancelled(cancellation)?;
            let answer = turn
                .content
                .filter(|content| !content.trim().is_empty())
                .ok_or(PromptError::EmptyAnswer)?;
            return Ok(PromptOutcome {
                answer,
                model_turns,
                script_calls,
                capability_invocations,
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

        for (tool_call_index, call) in turn.tool_calls.into_iter().enumerate() {
            check_cancelled(cancellation)?;
            let tool_call_index = tool_call_index + 1;
            if call.id.trim().is_empty() {
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
            if call.function.name == ASSET_TOOL_NAME
                && let Some(source) = assets
            {
                fetch_asset_into(&mut messages, source, &call, model_turns, tool_call_index)?;
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
                .saturating_sub(capability_invocations);
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
                        target: "dekopon_agent::audit",
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
                let outcome = runtime.run_script(&script, remaining);
                check_cancelled(cancellation)?;
                if dekopon_core::telemetry_payloads() {
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
                }
                outcome
            };
            script_calls = script_calls.saturating_add(1);
            capability_invocations =
                capability_invocations.saturating_add(outcome.capability_calls);
            messages.push(ModelMessage::tool(call.id, format_script_outcome(&outcome)));
        }
    }

    Err(PromptError::MaxSteps {
        maximum: limits.max_steps,
    })
}

fn check_cancelled(cancellation: Option<&dyn CancellationProbe>) -> Result<(), PromptError> {
    if cancellation.is_some_and(CancellationProbe::is_cancelled) {
        Err(PromptError::Cancelled)
    } else {
        Ok(())
    }
}

/// Renders one script outcome the way a terminal would: output, then an exit-code trailer.
///
/// `dekopon-run shell` prints this exact shape to a human and the prompt loop hands this exact
/// shape to a model, so a script a model wrote behaves identically when an operator reruns it.
#[must_use]
pub fn format_script_outcome(outcome: &ScriptOutcome) -> String {
    let mut text = outcome.output.clone();
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&format!("[exit code: {}]", outcome.exit_code));
    text
}

/// Stable outcome label for one script run.
///
/// A script that exits non-zero is a *reported* failure, not a broken session, so this is the
/// script's own exit status rather than a host error. Truncation is a separate dimension and is
/// exported as its own attribute.
#[must_use]
pub fn script_outcome_label(outcome: &ScriptOutcome) -> &'static str {
    if outcome.exit_code.get() == 0 {
        "succeeded"
    } else {
        "failed"
    }
}

/// Records one model-authored tool call the loop refused to run.
///
/// Every caller passes a fixed category rather than the model's own text: a rejection event is
/// triggered by untrusted model output, and `docs/observability.md` keeps that output out of
/// exported telemetry.
fn reject_tool_call(model_turn: u32, tool_call_index: usize, error_type: &'static str) {
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

/// Builds the scripting tool every prompt session offers.
///
/// `command_words` are the words loaded providers contribute on top of the fixed builtins. They are
/// appended rather than interpolated into the prose so the description stays one constant plus a
/// list, and so a session with no providers reads exactly as it did before.
fn script_tool(command_words: &[String]) -> ModelTool {
    let mut description = SCRIPT_TOOL_DESCRIPTION.to_owned();
    if !command_words.is_empty() {
        description.push_str(&format!(
            "\n\nThis session's providers add these command words: {}. Each takes its own arguments; `cap --describe` does not cover them.",
            command_words.join(", ")
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

fn agent_config_tool() -> ModelTool {
    ModelTool {
        name: AGENT_CONFIG_TOOL_NAME.to_owned(),
        description: "Inspect this session's credential-free agent configuration. Call this when \
                      asked about the agent's prompt, configuration, Cedar policy, permissions, \
                      tools, limits, or memory. The result contains the exact standing \
                      instructions, route/session bounds, and only the capabilities Cedar \
                      currently grants this sender through this agent. Present it as concise \
                      Markdown tables unless raw JSON was requested. Raw Cedar source, policy \
                      identifiers, principals, subjects, endpoints, paths, credential names, and \
                      all credential values are intentionally omitted."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

/// What a repeated `inspect_agent_config` call is answered with.
///
/// The configuration cannot change inside one session — it is built once, from one fresh broker
/// answer — so a second copy would say exactly what the first said. It would also stay in the
/// message vector and be re-sent to the provider on every remaining turn, which is why the
/// repeat is a pointer rather than a bounded-but-large duplicate.
const AGENT_CONFIG_ALREADY_SHOWN: &str = "This session's agent configuration is already in this \
                                          conversation, in the earlier inspect_agent_config \
                                          result. It cannot change within a session; read that \
                                          result again.";

/// Answers one `inspect_agent_config` call without touching the capability budget or broker.
///
/// `already_shown` is the session's own record of whether a full copy is already in `messages`.
/// Inspection stays repeatable under the loop's shared bounds; only the *bytes* are spent once.
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

/// Requires the meta tool's argument object to be exactly empty.
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

/// Media types whose bytes are readable as a tool result rather than as an attachment.
///
/// A model reads these as text, so routing them through an attachment part would encode a file it
/// could simply have been handed. Everything else — an image, a PDF, an office document — has to
/// arrive as a content part instead.
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
        description: "Look at a file someone attached to their chat message. The conversation \
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

/// Answers one `fetch_chat_asset` call by appending the tool result and, when the asset is not
/// text, the message that actually carries it.
///
/// Two messages rather than one because **a tool result cannot carry an attachment**. Chat
/// Completions types a `tool` message's content as a string, and the Responses API types
/// `function_call_output.output` the same way; neither accepts an image part where a tool result
/// goes. So the tool result says what happened and a following `user` message carries the bytes.
/// This shape is the only one both wire formats accept — do not "simplify" it by attaching to the
/// tool result.
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
    // A refusal is an outcome the model reads, not a failed session. Its text is gateway-authored
    // rather than sender-supplied, so it is safe to record.
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
    let text = is_textual(&asset.mime).then(|| String::from_utf8_lossy(&asset.data).into_owned());
    let truncated = text
        .as_ref()
        .is_some_and(|text| text.len() > MAX_TEXTUAL_ASSET_BYTES);
    // Size and media type, never the bytes and never the sender's file name, which is untrusted.
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

/// Clamps one textual asset to what a prompt can carry, saying so in the text itself.
///
/// The trailer is part of the tool result rather than a separate signal because the model is the
/// one that has to act on it: it can read what it got, and tell the person the rest was too large
/// to look at. That is the asset contract — an unusable attachment is refused in words the model
/// can pass on, never by failing the session.
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

/// Extracts the `id` argument from one `fetch_chat_asset` call.
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
    // Models write `5` and `"5"` for the same intent, and refusing the second would spend a turn
    // teaching one that the conversation already told it the number.
    let id = arguments.get("id").and_then(|id| match id {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    });
    id.ok_or_else(|| PromptError::MissingAssetId {
        tool: tool.to_owned(),
    })
}

/// Extracts the `script` argument from one model tool call.
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

/// The whole model-facing surface of a Dekopon session.
///
/// This replaces one JSON Schema per capability, so it is allowed to be long: it is paid once per
/// request instead of once per capability, and it shrinks rather than grows as an operator grants
/// more. What it must *not* do is describe anything the interpreter does not have. There is no
/// `help` builtin — the runtime discovery surface is `cap --list` and `cap --describe`, and
/// pointing a model at anything else would spend a tool call on "command not found".
const SCRIPT_TOOL_DESCRIPTION: &str = "\
Run one script in Dekopon's sandboxed shell. Returns the script's combined output followed by an \
`[exit code: N]` trailer, exactly as a terminal would.

The dialect is eerily close to bash and explicitly not bash. Pipelines, `&&`, `||`, `;`, a leading \
`!`, `if`/`elif`/`else`, `for`, `while`, `until`, `case`/`esac`, `break`/`continue`, functions \
with `$1`/`$@`/`$#`/`shift`/`local`, `$NAME`, `${NAME[index]}`, `$( )`, `$(( ))`, `$?`, `return`, \
`exit`, both quoting forms, here-documents (`<<EOF`, `<<-EOF`, and literal `<<'EOF'`), and \
`>`/`>>` into named in-memory buffers all behave the way you expect. Everything outside that \
curated set fails loudly and by name: `eval`, backticks, subshells, `[[ ]]`, `set -e`, `2>&1`, \
`<<<`, and `&` backgrounding are errors, never silent no-ops. If a script ran, it did what it said.

Four things genuinely differ from a real shell:

1. Commands are Dekopon capabilities, not programs. A command word containing `.`, `-`, or `_` is \
a capability invocation; every other word is a builtin. There are no processes, no filesystem, no \
environment variables, and no network reachable except through a capability.
2. Capability arguments are `--kebab-case` flags that become one JSON object: \
`posts.get --post-id 7 --include-body` sends `{\"postId\": 7, \"includeBody\": true}`. A repeated \
flag becomes an array, and a single bare `{...}` argument is used as the input verbatim.
3. Values are JSON, not text. `|` hands a structured value to the next command, and `jq` is built \
in to work on it.
4. The session is bounded. Steps, output, wall-clock time, and capability calls all have ceilings; \
tripping one ends the script with a message naming it.

Builtins: `jq`, `curl`, `gh`, `cap`, `cat`, `echo`, `printf`, `test`/`[`, `true`, `false`, \
`sleep`, `date`, `grep`, `sed`, `cut`, `sort`, `uniq`, `wc`, `base64`, `xargs`. Three of them \
depend on session configuration and report their exact missing prerequisite otherwise: `curl`, \
which opens no socket of its own but assembles a request for whichever HTTP capability the session \
was given; `gh`, which maps GitHub-CLI subcommands (`gh pr view 7 -R owner/repo`, `gh pr review 7 \
-R owner/repo --approve`) onto the correspondingly named granted `gh.*` capabilities; and `date`, \
which reads the host clock and renders `+%s` or an ISO-8601 instant. A provider may contribute \
further command words; any this session has are listed at the end of this description.

Patterns are literal text everywhere, never regular expressions or globs: a `grep`/`sed` pattern, \
and a `case` pattern too, where `*)` remains the default branch but `*.json)` is an error rather \
than a silent mismatch. Use `jq` for real matching. A here-document's body arrives as one JSON \
string, so pipe it through `jq` when you want structure out of it.

There is no `help`. Discover this session with `cap --list`, which returns a JSON array of the \
capability IDs you may invoke, and `cap --describe <capability>`, which returns one capability's \
input schema. Then prefer a single script that does the whole job over many small ones — that is \
the entire point of this tool.";

/// Failure to complete a prompt/tool session.
///
/// Every variant here is a broken *session*, not a failed script. A script that parses badly,
/// trips a budget, or calls a capability policy refuses is reported to the model through
/// [`format_script_outcome`] so it can recover.
#[derive(Debug, Error)]
pub enum PromptError {
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
            Self::Cancelled => "cancelled",
            Self::ZeroSteps => "zero-steps",
            Self::Model(_) => "model",
            Self::UnknownTool(_) => "unknown-tool",
            Self::TooManyToolCalls { .. } => "too-many-tool-calls",
            Self::EmptyToolCallId => "empty-tool-call-id",
            Self::InvalidArguments { .. } => "invalid-json-arguments",
            Self::ArgumentsNotObject { .. } => "arguments-not-object",
            Self::AgentConfigArgumentsNotEmpty { .. } => "agent-config-arguments-not-empty",
            Self::MissingScript { .. } => "missing-script",
            Self::MissingAssetId { .. } => "missing-asset-id",
            Self::EmptyAnswer => "empty-answer",
            Self::MaxSteps { .. } => "max-steps",
        }
    }
}

/// Renders the conversation so far for the transcript log.
///
/// Serialization failure is reported inline rather than propagated: telemetry must not be able to
/// end a session that is otherwise working.
/// Records reported token counts on the turn span, leaving unreported fields empty rather than
/// writing zeros the provider never sent.
fn record_usage(span: &tracing::Span, usage: &ModelUsage) {
    if let Some(tokens) = usage.input_tokens {
        span.record("usage.input_tokens", tokens);
    }
    if let Some(tokens) = usage.cached_input_tokens {
        span.record("usage.cached_input_tokens", tokens);
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

/// Renders requested tool calls for the transcript log.
fn tool_calls_json(tool_calls: &[ModelToolCall]) -> String {
    serde_json::to_string(tool_calls).unwrap_or_else(|_| "<unserializable>".to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use dekopon_model::model::{
        AssistantTurn, ChatModel, CompletionOptions, ModelError, ModelFunctionCall, ModelMessage,
        ModelTool, ModelToolCall, ModelUsage,
    };
    use dekopon_shell::{ExitCode, ScriptOutcome};
    use serde_json::{Value, json};

    use crate::meta::{
        AgentConfigView, ConversationConfigView, EffectiveCapabilityView, SessionConfigView,
    };

    use super::{
        AGENT_CONFIG_ALREADY_SHOWN, AGENT_CONFIG_TOOL_NAME, ASSET_TOOL_NAME, AssetSource,
        CancellationProbe, ConversationTurn, DEFAULT_MAX_BYTES, DEFAULT_MAX_TURNS, FetchedAsset,
        History, HistoryLimits, MAX_TEXTUAL_ASSET_BYTES, MAX_TOOL_CALLS_PER_TURN,
        ModelUsageObserver, PromptError, PromptLimits, SCRIPT_TOOL_NAME, ScriptRuntime,
        SessionInputs, agent_config_tool, format_script_outcome, run_prompt, run_prompt_session,
        run_prompt_with_history, run_prompt_with_history_and_options, script_tool,
    };

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
        }

        /// `(role, content)` pairs from the first request, the shape most assertions want.
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
        ) -> Result<AssistantTurn, ModelError> {
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

    /// Asserts a replayed window is a request both backends accept.
    ///
    /// The two 400s this feature could produce are a `tool` result whose call was trimmed away and
    /// an assistant `tool_calls` nothing answered. Neither is checked by reading the loop: both are
    /// checked on the serialized message, because the serialized message is what a backend sees.
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
            // A replayed message must carry its text on the wire. The ChatGPT backend emits an
            // assistant message that carries provider replay items as *only* those items and
            // discards its content, so a remembered answer reaching the request as anything other
            // than plain content would disappear from it without an error.
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

        // The session itself saw every tool result, and the conversation kept none of them: one
        // script's output can be 256 KiB, which is what replaying transcripts would cost.
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
            _messages: &[ModelMessage],
            _tools: &[ModelTool],
        ) -> Result<AssistantTurn, ModelError> {
            panic!("the loop must reach a model through complete_with");
        }

        fn complete_with(
            &self,
            _messages: &[ModelMessage],
            _tools: &[ModelTool],
            options: &CompletionOptions,
        ) -> Result<AssistantTurn, ModelError> {
            self.observed
                .lock()
                .expect("options lock")
                .push(options.prompt_cache_key().map(str::to_owned));
            self.turns
                .lock()
                .expect("turn lock")
                .pop_front()
                .ok_or(ModelError::NoChoices)
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
        let tool = script_tool(&["gh".to_owned(), "fly".to_owned()]);
        assert!(tool.description.contains("gh, fly"), "{}", tool.description);
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
            SessionInputs::new("inspect twice", limits(3, 32)).with_agent_config(&config),
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
            SessionInputs::new("what is in the file?", limits(3, 32)).with_assets(&assets),
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
}
