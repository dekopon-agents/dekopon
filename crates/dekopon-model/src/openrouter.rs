use dekopon_model_token_governor::ModelUsage;
pub mod settings;

use crate::{
    control::TurnControl,
    diagnostic::DiagnosticSecrets,
    error::{FailurePhase, InferenceError, ProtocolFailure, RequestError, TransportFailure},
    http::{InferenceHttp, Progress, record_phase},
    inference::{GenerateRequest, InferenceModel, ReplayEvent},
    model::{AssistantTurn, ModelKey, ModelMessage, ModelToolCall},
    openai::{
        CacheControl, ChunkError, FinishReason, OpenAiTool, WireMessage, WireToolCall,
        complete_turn, stream_failure,
    },
    sse::SseEvent,
    stream::{ModelText, TurnEvent},
};
use dekopon_core::Redacted;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use settings::{CacheStyle, Settings};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    num::NonZeroU32,
    ops::{ControlFlow, Range},
    time::Duration,
};

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

pub struct OpenRouterClient {
    http: InferenceHttp,
    endpoint: String,
    model: String,
    name: String,
    token: Redacted<String>,
    settings: Settings,
    identity: parking_lot::RwLock<ModelKey>,
}

impl OpenRouterClient {
    pub fn new(
        model: impl Into<String>,
        token: String,
        timeout: Duration,
        settings: Settings,
        extra_ca_bundles: &[dekopon_core::Redacted<String>],
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
            http: InferenceHttp::new(timeout, extra_ca_bundles)?,
            endpoint: ENDPOINT.into(),
            name: model.clone(),
            identity: parking_lot::RwLock::new(ModelKey::new(
                crate::model::Backend::OpenRouter,
                &model,
            )),
            model,
            token: Redacted::new(token),
            settings,
        })
    }

    pub fn model_key(&self) -> crate::model::ModelKey {
        self.identity.read().clone()
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_loopback_endpoint(mut self, endpoint: &str) -> Result<Self, InferenceError> {
        self.endpoint = crate::loopback::endpoint(endpoint)?;
        Ok(self)
    }

    #[cfg(test)]
    fn with_endpoint(mut self, endpoint: String) -> Self {
        self.endpoint = endpoint;
        self
    }

    #[cfg(test)]
    pub(crate) async fn prepare(
        &self,
        request: GenerateRequest<'_>,
    ) -> Result<Vec<u8>, InferenceError> {
        let chunks = self.prepare_for(request, &self.model_key()).await?;
        crate::body::collect(&chunks)
            .await
            .map_err(|error| TransportFailure::Body(error).into())
    }

    async fn prepare_for<'a>(
        &self,
        request: GenerateRequest<'a>,
        identity: &ModelKey,
    ) -> Result<Vec<crate::body::Chunk>, InferenceError> {
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
        let anchor = request
            .options
            .stable_system()
            .filter(|stable| (1..=leading).contains(stable))
            .unwrap_or(leading);
        let mut messages = Vec::with_capacity(request.messages.len());
        for (index, message) in request.messages.iter().enumerate() {
            if let ModelMessage::Journal { message } = message {
                messages.push(RouterMessage::Journal {
                    message,
                    model: identity.clone(),
                    cache: None,
                });
            } else if let ModelMessage::Assistant { turn } = message {
                if let Some(native) = turn
                    .openrouter_replay()
                    .filter(|_| turn.model_key().is_some_and(|key| key == identity))
                {
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
                    && index + 1 == anchor
                {
                    wire.mark_cache(cache.ttl)?;
                }
                messages.push(RouterMessage::Ordinary(wire));
            }
        }
        if let Some(cache) = explicit {
            mark_newest_text(request.messages, &mut messages, leading..usize::MAX, None);
            if let Some(stable) = request.options.stable_prefix() {
                mark_newest_text(request.messages, &mut messages, leading..stable, cache.ttl);
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
        let body = RouterRequest {
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
        };
        tracing::Span::current().record("tool_choice", body.tool_choice);
        let body = body.chunks().map_err(TransportFailure::Encoding)?;
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
        span.record("model.stream", true);
        span.record(
            "cache.style",
            match self.settings.cache.as_ref().map(|cache| &cache.style) {
                None | Some(CacheStyle::Automatic) => "automatic",
                Some(CacheStyle::ExplicitPrefix) => "explicitPrefix",
            },
        );
        if let Some(cache) = &self.settings.cache
            && let Some(ttl) = cache.ttl
        {
            span.record(
                "cache.ttl",
                match ttl {
                    settings::Ttl::FiveMinutes => "5m",
                    settings::Ttl::OneHour => "1h",
                },
            );
        }
    }

    async fn exchange(
        &self,
        request: GenerateRequest<'_>,
        observe: &mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
        control: &TurnControl,
        progress: &mut Progress,
        identity: &ModelKey,
    ) -> Result<AssistantTurn, InferenceError> {
        let secrets = DiagnosticSecrets::new(Some(&self.token));
        self.record_settings(secrets);
        let body = control
            .run(self.prepare_for(GenerateRequest { ..request }, identity))
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
                    .body(reqwest::Body::wrap(
                        crate::body::SliceBody::new(&body).map_err(TransportFailure::Body)?,
                    )),
                control,
                secrets,
            )
            .await?
            .check(control, secrets)
            .await?;
        response.require_sse(secrets)?;
        let mut state = RouterStream::new();
        let admitted = request
            .messages
            .iter()
            .any(|message| message.admits_opaque(identity));
        let mut verified = false;
        response
            .sse(control, &mut |event| {
                progress.event();
                match event {
                    SseEvent::Done => {
                        state.terminal = true;
                        Ok(ControlFlow::Break(()))
                    }
                    SseEvent::Data(data) => {
                        let chunk: RouterChunk = serde_json::from_str(data)
                            .map_err(|error| secrets.decode_failure(error))?;
                        let first_model = !verified && chunk.model.is_some();
                        if first_model {
                            let reported =
                                chunk.model.as_deref().map(|model| secrets.sanitize(model));
                            if admitted && identity.clone().with_reported(reported) != *identity {
                                return Err(ProtocolFailure::ContinuationMismatch.into());
                            }
                            verified = true;
                            if observe(ReplayEvent::ModelVerified).is_break() {
                                return Err(InferenceError::Cancelled);
                            }
                        }
                        state.apply(
                            chunk,
                            &mut |event| {
                                progress
                                    .observe(event, &mut |event| observe(ReplayEvent::Text(event)))
                            },
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

        let turn = state
            .finish(identity, secrets)
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        if let Some(key) = turn.model_key() {
            *self.identity.write() = key.clone();
        }
        Ok(turn)
    }
}

impl crate::inference::ReplayModel for OpenRouterClient {
    fn resolved_key(&self) -> ModelKey {
        self.model_key()
    }
    async fn exchange<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
        progress: &'a mut Progress,
        identity: &'a ModelKey,
    ) -> Result<AssistantTurn, InferenceError> {
        OpenRouterClient::exchange(self, request, observe, control, progress, identity).await
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
                async {
                    let mut progress = Progress::new();
                    (
                        crate::inference::retry_opaque(
                            self,
                            request,
                            observe,
                            control,
                            &mut progress,
                        )
                        .await,
                        progress,
                    )
                },
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

impl RouterRequest<'_> {
    fn chunks(&self) -> Result<Vec<crate::body::Chunk>, serde_json::Error> {
        let mut body = crate::body::JsonBody::default();
        body.raw(b"{\"model\":");
        body.value(self.model)?;
        body.raw(b",\"messages\":[");
        for (index, message) in self.messages.iter().enumerate() {
            if index > 0 {
                body.raw(b",");
            }
            match message {
                RouterMessage::Journal {
                    message,
                    model,
                    cache,
                } => message.chat(&mut body, Some(model), cache.as_ref())?,
                RouterMessage::Ordinary(_) | RouterMessage::PortableAssistant { .. } => {
                    body.value(message)?
                }
                RouterMessage::Native {
                    role,
                    content,
                    tool_calls,
                    reasoning_details,
                } => {
                    body.raw(b"{\"role\":");
                    body.value(role)?;
                    body.raw(b",\"content\":");
                    body.value(content)?;
                    if !tool_calls.is_empty() {
                        body.raw(b",\"tool_calls\":");
                        body.value(tool_calls)?;
                    }
                    if !reasoning_details.is_empty() {
                        body.raw(b",\"reasoning_details\":");
                        body.opaque(reasoning_details)?;
                    }
                    body.raw(b"}");
                }
            }
        }
        body.raw(b"],\"tools\":");
        body.value(&self.tools)?;
        body.raw(b",\"tool_choice\":");
        body.value(self.tool_choice)?;
        body.raw(b",\"stream\":true");
        if let Some(value) = self.max_tokens {
            body.raw(b",\"max_tokens\":");
            body.value(&value)?;
        }
        if let Some(value) = self.temperature {
            body.raw(b",\"temperature\":");
            body.value(&value)?;
        }
        if let Some(value) = self.top_p {
            body.raw(b",\"top_p\":");
            body.value(&value)?;
        }
        if let Some(value) = self.reasoning {
            body.raw(b",\"reasoning\":");
            body.value(value)?;
        }
        if let Some(value) = &self.provider {
            body.raw(b",\"provider\":");
            body.value(value)?;
        }
        if let Some(value) = self.session_id {
            body.raw(b",\"session_id\":");
            body.value(value)?;
        }
        body.raw(b"}");
        Ok(body.finish())
    }
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
    #[serde(skip)]
    Journal {
        message: &'a crate::replay::ReplayMessage,
        model: ModelKey,
        cache: Option<CacheControl>,
    },
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

#[derive(Clone, PartialEq)]
pub(crate) struct Replay {
    calls: Vec<Value>,
    reasoning: Vec<Value>,
}

impl Replay {
    pub(crate) fn has_reasoning(&self) -> bool {
        !self.reasoning.is_empty()
    }
    pub(crate) fn opaque_reasoning(
        &self,
    ) -> Result<Vec<crate::model::OpaqueReasoning>, serde_json::Error> {
        self.reasoning
            .iter()
            .map(crate::model::OpaqueReasoning::from_value)
            .collect()
    }
}

#[derive(Deserialize)]
struct RouterChunk {
    id: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    error: Option<ChunkError>,
    #[serde(default)]
    choices: Vec<RouterChoice>,
    usage: Option<RouterUsage>,
}

#[derive(Deserialize)]
struct RouterUsage {
    #[serde(flatten)]
    tokens: crate::wire::ChatUsage<u64>,
    #[serde(default)]
    cost: Value,
    #[serde(default)]
    cost_details: Value,
}

impl RouterUsage {
    fn record(&self) {
        let span = tracing::Span::current();
        let detail = |name| self.cost_details.get(name).and_then(Value::as_f64);
        for (field, value) in [
            ("usage.cost", self.cost.as_f64()),
            (
                "usage.upstream_inference_cost",
                detail("upstream_inference_cost"),
            ),
            (
                "usage.upstream_inference_prompt_cost",
                detail("upstream_inference_prompt_cost"),
            ),
            (
                "usage.upstream_inference_completions_cost",
                detail("upstream_inference_completions_cost"),
            ),
        ] {
            if let Some(value) = value {
                span.record(field, value);
            }
        }
    }
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
    #[serde(flatten)]
    other: BTreeMap<String, serde::de::IgnoredAny>,
}

struct RouterStream {
    content: String,
    replay: Replay,
    response_id: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    reasoning_positions: HashMap<u64, usize>,
    call_positions: HashMap<u64, usize>,
    usage: Option<ModelUsage>,
    reason: Option<FinishReason>,
    terminal: bool,
    choices_seen: bool,
    unrecognized: BTreeSet<String>,
}

fn mark_newest_text(
    messages: &[ModelMessage],
    wire: &mut [RouterMessage<'_>],
    range: Range<usize>,
    ttl: Option<settings::Ttl>,
) {
    for (message, wire) in messages
        .iter()
        .zip(wire)
        .take(range.end)
        .skip(range.start)
        .rev()
    {
        match wire {
            RouterMessage::Journal { message, cache, .. } => {
                if message.cacheable() {
                    *cache = Some(CacheControl::new(ttl));
                    return;
                }
            }
            RouterMessage::Ordinary(wire) => {
                if matches!(
                    message,
                    ModelMessage::User { .. } | ModelMessage::ToolResults { .. }
                ) && wire.mark_last_text(ttl)
                {
                    return;
                }
            }
            RouterMessage::PortableAssistant { .. } | RouterMessage::Native { .. } => {}
        }
    }
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
            response_id: None,
            model: None,
            provider: None,
            reasoning_positions: HashMap::new(),
            call_positions: HashMap::new(),
            usage: None,
            reason: None,
            terminal: false,
            choices_seen: false,
            unrecognized: BTreeSet::new(),
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
        if self.response_id.is_none()
            && let Some(id) = chunk.id
        {
            tracing::Span::current().record("gen_ai.response.id", secrets.sanitize(&id));
            self.response_id = Some(id);
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
            usage.record();
            self.usage = Some(usage.tokens.into_openrouter());
        }
        for choice in chunk.choices {
            self.choices_seen = true;
            if let Some(reason) = choice.finish_reason {
                reason.record(secrets)?;
                self.reason = Some(reason);
                self.terminal = true;
            }
            let delta = choice.delta.unwrap_or_default();
            for key in delta.other.into_keys() {
                self.unrecognized.insert(key);
                if self.unrecognized.len() > 8 {
                    self.unrecognized.pop_last();
                }
            }
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
                merge_tool(
                    &mut self.replay.calls,
                    &mut self.call_positions,
                    item,
                    secrets,
                )?;
            }
        }
        Ok(())
    }

    fn finish(
        mut self,
        identity: &ModelKey,
        secrets: DiagnosticSecrets<'_>,
    ) -> Result<AssistantTurn, InferenceError> {
        let span = tracing::Span::current();
        span.record("reasoning.items", self.replay.reasoning.len());
        if !self.unrecognized.is_empty() {
            span.record(
                "delta.unrecognized",
                secrets.sanitize(&self.unrecognized.into_iter().collect::<Vec<_>>().join(",")),
            );
        }
        if !self.terminal {
            return Err(ProtocolFailure::MissingTerminal.into());
        }
        if !self.choices_seen {
            return Err(ProtocolFailure::NoChoices.into());
        }
        let mut calls = Vec::with_capacity(self.replay.calls.len());
        let mut kept = Vec::with_capacity(self.replay.calls.len());
        let mut skipped = 0_u64;
        for mut value in std::mem::take(&mut self.replay.calls) {
            let object = value
                .as_object_mut()
                .ok_or_else(|| protocol("tool call must be an object", secrets))?;
            object.remove("index");
            if let Some(call) = WireToolCall::deserialize(&value)
                .map_err(|error| secrets.decode_failure(error))?
                .into_model(secrets)?
            {
                calls.push(call);
                kept.push(value);
            } else {
                skipped += 1;
            }
        }
        span.record("tool_call.skipped", skipped);
        self.replay.calls = kept;
        Ok(complete_turn(
            (!self.content.is_empty()).then_some(self.content),
            calls,
            self.usage,
            self.reason.as_ref(),
        )
        .with_openrouter_continuation(
            identity.clone().with_reported(
                self.model
                    .as_deref()
                    .or_else(|| identity.reported())
                    .map(|model| secrets.sanitize(model)),
            ),
            self.replay,
        ))
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
) -> Result<(), InferenceError> {
    let Value::Object(mut object) = incoming else {
        return Err(protocol("tool fragment must be an object", secrets));
    };
    let index = item_index(&object, secrets)?.unwrap_or(0);
    object.insert("index".into(), index.into());
    let Some(&position) = positions.get(&index) else {
        positions.insert(index, items.len());
        items.push(Value::Object(object));
        return Ok(());
    };
    let target = items
        .get_mut(position)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| protocol("tool fragment must be an object", secrets))?;
    let kind = object
        .get("type")
        .filter(|value| !value.is_null())
        .or_else(|| target.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("function");
    if kind != "function" {
        merge_fields(target, object, &[], secrets)?;
        return Ok(());
    }
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
    Ok(())
}

#[cfg(test)]
mod tests;
