//! OpenRouter's streaming dialect, explicit cache anchors and private native replay.

/// Strict authored settings shared with gateway configuration.
pub mod settings;

use crate::{
    control::TurnControl,
    diagnostic::DiagnosticSecrets,
    error::{FailurePhase, InferenceError, ProtocolFailure, RequestError, TransportFailure},
    http::{InferenceHttp, Progress, record_phase},
    inference::{GenerateRequest, InferenceModel},
    model::{
        AssistantTurn, ClientIdentity, ModelMessage, ModelToolCall, ModelUsage, compact_json_body,
    },
    openai::{
        ChunkError, FinishReason, OpenAiTool, WireChatUsage, WireMessage, WireToolCall,
        complete_turn, stream_failure,
    },
    sse::SseEvent,
    stream::{ModelText, TurnEvent},
};
use dekopon_core::Redacted;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use settings::{CacheStyle, Settings};
use std::{collections::HashMap, num::NonZeroU32, ops::ControlFlow, time::Duration};

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

/// A pooled OpenRouter client with immutable authored settings and a caller-supplied credential.
pub struct OpenRouterClient {
    http: InferenceHttp,
    endpoint: String,
    model: String,
    name: String,
    token: Redacted<String>,
    settings: Settings,
    identity: ClientIdentity,
}

impl OpenRouterClient {
    /// Validates local settings and wraps the credential before constructing the transport.
    pub fn new(
        model: impl Into<String>,
        token: String,
        timeout: Duration,
        settings: Settings,
    ) -> Result<Self, InferenceError> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(RequestError::EmptyModel.into());
        }
        if token.trim().is_empty() {
            return Err(RequestError::EmptyCredential.into());
        }
        if let Some(problem) = settings.problems().first() {
            return Err(RequestError::OpenRouterSetting(*problem).into());
        }
        Ok(Self {
            http: InferenceHttp::new(timeout)?,
            endpoint: ENDPOINT.into(),
            name: model.clone(),
            model,
            token: Redacted::new(token),
            settings,
            identity: ClientIdentity::new(),
        })
    }

    /// Records the configured name separately from the requested upstream model.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Overrides generation for an offline fixture at literal `127.0.0.1` or `::1` over HTTP.
    /// Existing continuations are invalidated when changing the configured destination.
    pub fn with_loopback_endpoint(mut self, endpoint: &str) -> Result<Self, InferenceError> {
        self.endpoint = crate::loopback::endpoint(endpoint)?;
        self.identity = ClientIdentity::new();
        Ok(self)
    }

    #[cfg(test)]
    fn with_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = endpoint;
        self
    }

    async fn prepare<'a>(&self, request: GenerateRequest<'a>) -> Result<Vec<u8>, InferenceError> {
        let explicit = self
            .settings
            .cache
            .as_ref()
            .filter(|cache| cache.style == CacheStyle::ExplicitPrefix);
        let leading = request
            .messages
            .iter()
            .take_while(|message| matches!(message, ModelMessage::System { .. }))
            .count();
        if explicit.is_some() && leading == 0 {
            return Err(RequestError::CacheAnchor.into());
        }
        let mut messages = Vec::with_capacity(request.messages.len());
        for (index, message) in request.messages.iter().enumerate() {
            if let ModelMessage::Assistant { turn } = message {
                if !turn.accepts_client(&self.identity) {
                    return Err(ProtocolFailure::ContinuationMismatch.into());
                }
                if let Some(native) = turn.openrouter_replay() {
                    messages.push(RouterMessage::Native {
                        role: "assistant",
                        content: turn.content.as_deref(),
                        tool_calls: &native.calls,
                        reasoning_details: &native.reasoning,
                    });
                } else {
                    messages.push(RouterMessage::PortableAssistant {
                        role: "assistant",
                        content: turn.content.as_deref(),
                        tool_calls: &turn.tool_calls,
                    });
                }
            } else {
                let mut wire = WireMessage::prepare(message).await?;
                if let Some(cache) = explicit
                    && index + 1 == leading
                {
                    wire.mark_cache(cache.ttl)?;
                }
                messages.push(RouterMessage::Ordinary(wire));
            }
        }
        let tools = request
            .tools
            .iter()
            .map(|function| OpenAiTool {
                kind: "function",
                function,
            })
            .collect::<Vec<_>>();
        let generation = self.settings.generation.as_ref();
        let provider = self.settings.routing.as_ref().map(|routing| Provider {
            allow_fallbacks: routing.allow_fallbacks,
            require_parameters: routing.require_parameters,
            only: routing.only.as_deref(),
        });
        let body = compact_json_body(&RouterRequest {
            model: &self.model,
            messages,
            tools,
            tool_choice: "auto",
            stream: true,
            max_tokens: generation.and_then(|settings| settings.max_output_tokens),
            temperature: generation.and_then(|settings| settings.temperature),
            top_p: generation.and_then(|settings| settings.top_p),
            reasoning: self.settings.reasoning.as_ref(),
            provider,
            session_id: request.options.prompt_cache_key(),
        })
        .map_err(TransportFailure::Encoding)?;
        Ok(body)
    }

    fn record_settings(&self, secrets: DiagnosticSecrets<'_>) {
        let span = tracing::Span::current();
        if let Some(generation) = &self.settings.generation {
            if let Some(value) = generation.max_output_tokens {
                span.record("generation.max_output_tokens", value);
            }
            if let Some(value) = generation.temperature {
                span.record("generation.temperature", value);
            }
            if let Some(value) = generation.top_p {
                span.record("generation.top_p", value);
            }
        }
        if let Some(reasoning) = &self.settings.reasoning {
            span.record(
                "reasoning.effort",
                match reasoning.effort {
                    settings::Effort::None => "none",
                    settings::Effort::Minimal => "minimal",
                    settings::Effort::Low => "low",
                    settings::Effort::Medium => "medium",
                    settings::Effort::High => "high",
                    settings::Effort::Xhigh => "xhigh",
                    settings::Effort::Max => "max",
                },
            );
        }
        if let Some(routing) = &self.settings.routing {
            if let Some(value) = routing.allow_fallbacks {
                span.record("routing.allow_fallbacks", value);
            }
            if let Some(value) = routing.require_parameters {
                span.record("routing.require_parameters", value);
            }
            if let Some(only) = &routing.only {
                span.record("routing.only", secrets.sanitize(&only.join(",")));
            }
        }
        if let Some(cache) = &self.settings.cache {
            span.record(
                "cache.style",
                match cache.style {
                    CacheStyle::Automatic => "automatic",
                    CacheStyle::ExplicitPrefix => "explicitPrefix",
                },
            );
            if let Some(ttl) = cache.ttl {
                span.record(
                    "cache.ttl",
                    match ttl {
                        settings::Ttl::FiveMinutes => "5m",
                        settings::Ttl::OneHour => "1h",
                    },
                );
            }
        }
    }

    async fn exchange(
        &self,
        request: GenerateRequest<'_>,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        let mut progress = Progress::new();
        let secrets = DiagnosticSecrets::new(Some(&self.token));
        self.record_settings(secrets);
        let body = control
            .run(self.prepare(request))
            .await
            .and_then(|result| result)
            .inspect_err(|_| record_phase(FailurePhase::BeforeSend))?;
        let response = self
            .http
            .send(
                self.http
                    .post(&self.endpoint)
                    .bearer_auth(self.token.expose())
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream")
                    .header("X-OpenRouter-Cache", "false")
                    .body(body),
                control,
                secrets,
            )
            .await?
            .check(control, secrets)
            .await?;
        response.require_sse(secrets)?;
        let mut state = RouterStream::new();
        response
            .sse(control, &mut |event| {
                progress.event();
                match event {
                    SseEvent::Done => {
                        state.terminal = true;
                        Ok(ControlFlow::Break(()))
                    }
                    SseEvent::Data(data) => {
                        let chunk = serde_json::from_str(data)
                            .map_err(|error| secrets.decode_failure(error))?;
                        state.apply(
                            chunk,
                            &mut |event| progress.observe(event, observe),
                            secrets,
                        )?;
                        Ok(ControlFlow::Continue(()))
                    }
                }
            })
            .await?;
        control
            .check()
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        state
            .finish(&self.identity, secrets)
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))
    }
}

impl InferenceModel for OpenRouterClient {
    async fn generate<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        self.http
            .generation(
                &self.name,
                &self.model,
                "openrouter",
                "chat-completions",
                self.exchange(request, observe, control),
            )
            .await
    }
}

#[derive(Serialize)]
struct RouterRequest<'a> {
    model: &'a str,
    messages: Vec<RouterMessage<'a>>,
    tools: Vec<OpenAiTool<'a>>,
    tool_choice: &'static str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<NonZeroU32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<&'a settings::Reasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<Provider<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

#[derive(Serialize)]
struct Provider<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_fallbacks: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    require_parameters: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    only: Option<&'a [String]>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum RouterMessage<'a> {
    Ordinary(WireMessage<'a>),
    PortableAssistant {
        role: &'static str,
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "<[ModelToolCall]>::is_empty")]
        tool_calls: &'a [ModelToolCall],
    },
    Native {
        role: &'static str,
        content: Option<&'a str>,
        #[serde(skip_serializing_if = "<[Value]>::is_empty")]
        tool_calls: &'a [Value],
        #[serde(skip_serializing_if = "<[Value]>::is_empty")]
        reasoning_details: &'a [Value],
    },
}

/// Bounded native subtrees are intentionally excluded from the turn's Debug/audit projections.
#[derive(Clone, PartialEq)]
pub(crate) struct Replay {
    calls: Vec<Value>,
    reasoning: Vec<Value>,
}

#[derive(Deserialize)]
struct RouterChunk {
    model: Option<String>,
    provider: Option<String>,
    error: Option<ChunkError>,
    #[serde(default)]
    choices: Vec<RouterChoice>,
    usage: Option<WireChatUsage<u64>>,
}

#[derive(Deserialize)]
struct RouterChoice {
    delta: Option<Delta>,
    finish_reason: Option<FinishReason>,
}

#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    #[serde(default)]
    reasoning_details: Option<Vec<Value>>,
    #[serde(default)]
    tool_calls: Option<Vec<Value>>,
}

struct RouterStream {
    content: String,
    replay: Replay,
    model: Option<String>,
    provider: Option<String>,
    reasoning_positions: HashMap<u64, usize>,
    call_positions: HashMap<u64, usize>,
    usage: Option<ModelUsage>,
    reason: Option<FinishReason>,
    terminal: bool,
    choices_seen: bool,
}

fn protocol(message: &str, secrets: DiagnosticSecrets<'_>) -> InferenceError {
    ProtocolFailure::OpenRouter(secrets.sanitize(message)).into()
}

impl RouterStream {
    fn new() -> Self {
        Self {
            content: String::new(),
            replay: Replay {
                calls: Vec::new(),
                reasoning: Vec::new(),
            },
            model: None,
            provider: None,
            reasoning_positions: HashMap::new(),
            call_positions: HashMap::new(),
            usage: None,
            reason: None,
            terminal: false,
            choices_seen: false,
        }
    }

    fn apply(
        &mut self,
        chunk: RouterChunk,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        secrets: DiagnosticSecrets<'_>,
    ) -> Result<(), InferenceError> {
        if let Some(error) = chunk.error {
            return Err(stream_failure(error, secrets));
        }
        for (field, slot, value) in [
            ("model.returned", &mut self.model, chunk.model),
            ("model.upstream", &mut self.provider, chunk.provider),
        ] {
            if let Some(value) = value
                && slot.as_ref() != Some(&value)
            {
                tracing::Span::current().record(field, secrets.sanitize(&value));
                *slot = Some(value);
            }
        }
        if let Some(usage) = chunk.usage {
            self.usage = Some(usage.into_openrouter());
        }
        for choice in chunk.choices {
            self.choices_seen = true;
            if let Some(reason) = choice.finish_reason {
                reason.record(secrets)?;
                self.reason = Some(reason);
                self.terminal = true;
            }
            let delta = choice.delta.unwrap_or_default();
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                self.content.push_str(&text);
                if observe(TurnEvent::TextDelta(ModelText::from_model(text))).is_break() {
                    return Err(InferenceError::Cancelled);
                }
            }
            for item in delta.reasoning_details.unwrap_or_default() {
                merge_reasoning(
                    &mut self.replay.reasoning,
                    &mut self.reasoning_positions,
                    item,
                    secrets,
                )?;
            }
            for item in delta.tool_calls.unwrap_or_default() {
                let started = merge_tool(
                    &mut self.replay.calls,
                    &mut self.call_positions,
                    item,
                    secrets,
                )?;
                if started
                    && observe(TurnEvent::ToolCallStarted {
                        index: u32::try_from(self.replay.calls.len() - 1).unwrap_or(u32::MAX),
                    })
                    .is_break()
                {
                    return Err(InferenceError::Cancelled);
                }
            }
        }
        Ok(())
    }

    fn finish(
        mut self,
        identity: &ClientIdentity,
        secrets: DiagnosticSecrets<'_>,
    ) -> Result<AssistantTurn, InferenceError> {
        if !self.terminal {
            return Err(ProtocolFailure::MissingTerminal.into());
        }
        if !self.choices_seen {
            return Err(ProtocolFailure::NoChoices.into());
        }
        let mut calls = Vec::with_capacity(self.replay.calls.len());
        for value in &mut self.replay.calls {
            let object = value
                .as_object_mut()
                .ok_or_else(|| protocol("tool call must be an object", secrets))?;
            object.remove("index");
            let call = WireToolCall::deserialize(&*value)
                .map_err(|error| secrets.decode_failure(error))?
                .into_model(secrets)?;
            calls.push(call);
        }
        Ok(complete_turn(
            (!self.content.is_empty()).then_some(self.content),
            calls,
            self.usage,
            self.reason.as_ref(),
        )
        .with_openrouter_continuation(identity.clone(), self.replay))
    }
}

fn item_index(
    item: &Map<String, Value>,
    secrets: DiagnosticSecrets<'_>,
) -> Result<Option<u64>, InferenceError> {
    match item.get("index") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| protocol("native index must be an unsigned integer", secrets)),
    }
}

fn merge_fields(
    target: &mut Map<String, Value>,
    incoming: Map<String, Value>,
    fragments: &[&str],
    secrets: DiagnosticSecrets<'_>,
) -> Result<(), InferenceError> {
    for (key, value) in incoming {
        if fragments.contains(&key.as_str()) && !value.is_null() {
            let text = value
                .as_str()
                .ok_or_else(|| protocol("native fragment must be text", secrets))?;
            match target.get_mut(&key) {
                Some(Value::String(prior)) => prior.push_str(text),
                None | Some(Value::Null) => {
                    target.insert(key, value);
                }
                _ => return Err(protocol("native fragment must be text", secrets)),
            }
        } else if target.get(&key).is_none_or(Value::is_null) {
            target.insert(key, value);
        }
    }
    Ok(())
}

fn merge_reasoning(
    items: &mut Vec<Value>,
    positions: &mut HashMap<u64, usize>,
    incoming: Value,
    secrets: DiagnosticSecrets<'_>,
) -> Result<(), InferenceError> {
    let object = incoming
        .as_object()
        .ok_or_else(|| protocol("reasoning detail must be an object", secrets))?;
    let index = item_index(object, secrets)?;
    if let Some(index) = index
        && let Some(&position) = positions.get(&index)
    {
        let Value::Object(incoming) = incoming else {
            return Err(protocol("reasoning detail must be an object", secrets));
        };
        let target = items
            .get_mut(position)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| protocol("reasoning detail must be an object", secrets))?;
        merge_fields(target, incoming, &["text", "summary", "data"], secrets)?;
    } else {
        if let Some(index) = index {
            positions.insert(index, items.len());
        }
        items.push(incoming);
    }
    Ok(())
}

fn merge_tool(
    items: &mut Vec<Value>,
    positions: &mut HashMap<u64, usize>,
    incoming: Value,
    secrets: DiagnosticSecrets<'_>,
) -> Result<bool, InferenceError> {
    let Value::Object(mut object) = incoming else {
        return Err(protocol("tool fragment must be an object", secrets));
    };
    let index = item_index(&object, secrets)?.unwrap_or(0);
    object.insert("index".into(), index.into());
    let Some(&position) = positions.get(&index) else {
        positions.insert(index, items.len());
        items.push(Value::Object(object));
        return Ok(true);
    };
    let target = items
        .get_mut(position)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| protocol("tool fragment must be an object", secrets))?;
    if let Some(function) = object.remove("function").filter(|value| !value.is_null()) {
        let Value::Object(function) = function else {
            return Err(protocol("function fragment must be an object", secrets));
        };
        let existing = target
            .entry("function")
            .or_insert_with(|| Value::Object(Map::new()));
        if existing.is_null() {
            *existing = Value::Object(Map::new());
        }
        let existing = existing
            .as_object_mut()
            .ok_or_else(|| protocol("function fragment must be an object", secrets))?;
        merge_fields(existing, function, &["arguments"], secrets)?;
    }
    merge_fields(target, object, &[], secrets)?;
    Ok(false)
}

#[cfg(test)]
mod tests;
