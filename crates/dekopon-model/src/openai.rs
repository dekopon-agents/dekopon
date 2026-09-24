use crate::{
    control::TurnControl,
    diagnostic::DiagnosticSecrets,
    error::{
        InferenceError, ProtocolFailure, ProviderFailure, RequestError, TransportFailure,
        UnsupportedFeature,
    },
    http::{InferenceHttp, Progress, record_phase},
    inference::{GenerateRequest, InferenceModel},
    model::{
        AssistantTurn, ContentPart, DataUrl, JSON_CONTENT_TYPE, ModelFunctionCall, ModelMessage,
        ModelTool, ModelToolCall, ModelUsage, compact_json_body,
    },
    sse::{SseEvent, decode_transcript},
    stream::{ModelText, TurnEvent},
};
use dekopon_core::Redacted;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, ops::ControlFlow, time::Duration};

pub struct OpenAiClient {
    http: InferenceHttp,
    endpoint: String,
    model: String,
    name: String,
    bearer_token: Option<Redacted<String>>,
    stream: bool,
}

impl OpenAiClient {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        bearer_token: Option<String>,
        timeout: Duration,
    ) -> Result<Self, InferenceError> {
        if timeout.is_zero() {
            return Err(RequestError::ZeroTimeout.into());
        }
        let endpoint = endpoint.into();
        if endpoint.trim().is_empty() {
            return Err(RequestError::EmptyEndpoint.into());
        }
        let model = model.into();
        if model.trim().is_empty() {
            return Err(RequestError::EmptyModel.into());
        }
        let bearer_token = bearer_token.and_then(|token| {
            let token = token.trim().to_owned();
            (!token.is_empty()).then_some(Redacted::new(token))
        });
        if bearer_token.is_some() && !allows_bearer_token(&endpoint) {
            return Err(RequestError::InsecureBearer.into());
        }
        Ok(Self {
            http: InferenceHttp::new(timeout)?,
            endpoint: completion_url(&endpoint),
            name: model.clone(),
            model,
            bearer_token,
            stream: true,
        })
    }

    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    #[must_use]
    pub fn with_streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    async fn exchange(
        &self,
        request: GenerateRequest<'_>,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        tracing::Span::current().record("model.stream", self.stream);
        let mut progress = Progress::new();
        let secrets = DiagnosticSecrets::new(self.bearer_token.as_ref());
        let encoded = control
            .run(async {
                let mut messages = Vec::with_capacity(request.messages.len());
                for message in request.messages {
                    if let ModelMessage::Assistant { turn } = message
                        && !turn.is_portable()
                    {
                        return Err(ProtocolFailure::ContinuationMismatch.into());
                    }
                    messages.push(WireMessage::prepare(message).await?);
                }
                let tools = request
                    .tools
                    .iter()
                    .map(|function| OpenAiTool {
                        kind: "function",
                        function,
                    })
                    .collect::<Vec<_>>();
                compact_json_body(&ChatRequest {
                    model: &self.model,
                    messages: &messages,
                    tools: &tools,
                    tool_choice: "auto",
                    prompt_cache_key: request.options.prompt_cache_key(),
                    stream: self.stream.then_some(true),
                    stream_options: self.stream.then_some(StreamOptions {
                        include_usage: true,
                    }),
                })
                .map_err(|error| InferenceError::Transport(TransportFailure::Encoding(error)))
            })
            .await
            .and_then(|result| result)
            .inspect_err(|_| record_phase(crate::error::FailurePhase::BeforeSend))?;
        let mut builder = self
            .http
            .post(&self.endpoint)
            .header(
                "accept",
                if self.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .header("content-type", JSON_CONTENT_TYPE)
            .body(encoded);
        if let Some(token) = &self.bearer_token {
            builder = builder.bearer_auth(token.expose());
        }
        let response = self
            .http
            .send(builder, control, secrets)
            .await?
            .check(control, secrets)
            .await?;
        if !self.stream {
            return response
                .buffered(control, |bytes| {
                    let response: ChatResponse = serde_json::from_slice(bytes)
                        .map_err(|error| secrets.decode_failure(error))?;
                    turn_from_response(secrets, response)
                })
                .await;
        }
        response.require_sse(secrets)?;
        let mut state = ChatStream::default();
        response
            .sse(control, &mut |event| {
                progress.event();
                match event {
                    SseEvent::Done => {
                        state.finished = true;
                        Ok(ControlFlow::Break(()))
                    }
                    SseEvent::Data(data) => {
                        let chunk = serde_json::from_str(data)
                            .map_err(|error| secrets.decode_failure(error))?;
                        if state
                            .apply(
                                chunk,
                                &mut |event| progress.observe(event, observe),
                                secrets,
                            )?
                            .is_break()
                        {
                            return Err(InferenceError::Cancelled);
                        }
                        Ok(ControlFlow::Continue(()))
                    }
                }
            })
            .await?;
        control
            .check()
            .inspect_err(|_| record_phase(crate::error::FailurePhase::ReadingBody))?;
        if !state.finished {
            record_phase(crate::error::FailurePhase::ReadingBody);
            return Err(ProtocolFailure::MissingTerminal.into());
        }
        state
            .into_turn(secrets)
            .inspect_err(|_| record_phase(crate::error::FailurePhase::ReadingBody))
    }
}

impl InferenceModel for OpenAiClient {
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
                "openai-compatible",
                "chat-completions",
                self.exchange(request, observe, control),
            )
            .await
    }
}

#[derive(Debug)]
pub(crate) enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Extension(String),
}

impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "stop" => Self::Stop,
            "tool_calls" => Self::ToolCalls,
            "length" => Self::Length,
            "content_filter" => Self::ContentFilter,
            _ => Self::Extension(value),
        })
    }
}

impl FinishReason {
    pub(crate) fn record(&self, secrets: DiagnosticSecrets<'_>) -> Result<(), InferenceError> {
        let value = match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_calls",
            Self::Length => "length",
            Self::ContentFilter => "content_filter",
            Self::Extension(value) => value,
        };
        tracing::Span::current().record("finish.reason", secrets.sanitize(value));
        if matches!(self, Self::ContentFilter) {
            return Err(ProviderFailure::new(200, "content_filter").into());
        }
        Ok(())
    }
}

pub(crate) fn complete_turn(
    content: Option<String>,
    calls: Vec<ModelToolCall>,
    usage: Option<ModelUsage>,
    reason: Option<&FinishReason>,
) -> AssistantTurn {
    tracing::Span::current().record(
        "output.partial",
        matches!(reason, Some(FinishReason::Length)),
    );
    AssistantTurn::new(content, calls, usage)
}

pub(crate) fn stream_failure(error: ChunkError, secrets: DiagnosticSecrets<'_>) -> InferenceError {
    let mut failure = ProviderFailure::new(
        200,
        &secrets.sanitize(error.message.as_deref().unwrap_or("no message")),
    );
    failure.code = error.code.as_ref().map(|code| code.sanitized(secrets));
    failure.into()
}

fn record_model(model: Option<&str>, secrets: DiagnosticSecrets<'_>) {
    if let Some(model) = model {
        tracing::Span::current().record("model.returned", secrets.sanitize(model));
    }
}

fn turn_from_response(
    secrets: crate::diagnostic::DiagnosticSecrets<'_>,
    response: ChatResponse,
) -> Result<AssistantTurn, InferenceError> {
    record_model(response.model.as_deref(), secrets);
    if let Some(error) = response.error {
        return Err(stream_failure(error, secrets));
    }
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(ProtocolFailure::NoChoices)?;
    if let Some(reason) = &choice.finish_reason {
        reason.record(secrets)?;
    }
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(|call| call.into_model(secrets))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(complete_turn(
        choice.message.content,
        tool_calls,
        response.usage.map(ModelUsage::from),
        choice.finish_reason.as_ref(),
    ))
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [WireMessage<'a>],
    tools: &'a [OpenAiTool<'a>],
    tool_choice: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    /// Never set stream to Some(false); only Some(true) or None keep wire compatibility for
    /// endpoints unaware of this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Kept separate from ModelMessage, whose Serialize is the redacted audit rendering, because when
/// they were one type a base64 attachment was one careless to_string away from being logged
/// forever.
#[derive(Debug, Serialize)]
pub(crate) struct WireMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<WireContent<'a>>,
    #[serde(skip_serializing_if = "is_empty")]
    tool_calls: &'a [ModelToolCall],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

fn is_empty(calls: &&[ModelToolCall]) -> bool {
    calls.is_empty()
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
    Text(&'a str),
    Parts(Vec<WirePart<'a>>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum WirePart<'a> {
    #[serde(rename = "text")]
    Text {
        text: std::borrow::Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireUrl<'a> },
    #[serde(rename = "file")]
    File { file: WireFile<'a> },
}

#[derive(Debug, Serialize)]
struct WireUrl<'a> {
    url: DataUrl<'a>,
}

#[derive(Debug, Serialize)]
struct WireFile<'a> {
    filename: &'a str,
    file_data: DataUrl<'a>,
}

#[derive(Debug, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<crate::openrouter::settings::Ttl>,
}

impl<'a> WireMessage<'a> {
    pub(crate) fn mark_cache(
        &mut self,
        ttl: Option<crate::openrouter::settings::Ttl>,
    ) -> Result<(), InferenceError> {
        let Some(WireContent::Text(text)) = &self.content else {
            return Err(RequestError::CacheAnchor.into());
        };
        self.content = Some(WireContent::Parts(vec![WirePart::Text {
            text: (*text).into(),
            cache_control: Some(CacheControl {
                kind: "ephemeral",
                ttl,
            }),
        }]));
        Ok(())
    }

    pub(crate) async fn prepare(message: &'a ModelMessage) -> Result<Self, InferenceError> {
        let content = if let Some(parts) = message.parts() {
            let mut wire = Vec::with_capacity(parts.len());
            for part in parts {
                let bytes = match part {
                    ContentPart::Image { data, .. } | ContentPart::File { data, .. } => {
                        let reference = data.clone();
                        let span = tracing::Span::current();
                        match tokio::task::spawn_blocking(move || {
                            span.in_scope(|| reference.read())
                        })
                        .await
                        .map_err(TransportFailure::Blocking)?
                        {
                            Ok(bytes) => Some(bytes),
                            Err(
                                crate::asset::BlobError::Reclaimed
                                | crate::asset::BlobError::Unauthorized,
                            ) => {
                                wire.push(WirePart::Text {
                                    text: data.release_notice().into(),
                                    cache_control: None,
                                });
                                continue;
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    ContentPart::Text(_) => None,
                };
                wire.push(match part {
                    ContentPart::Text(text) => WirePart::Text {
                        text: text.into(),
                        cache_control: None,
                    },
                    ContentPart::Image { mime, .. } => WirePart::ImageUrl {
                        image_url: WireUrl {
                            url: DataUrl::new(mime, bytes.unwrap_or_default()),
                        },
                    },
                    ContentPart::File { name, mime, .. } => WirePart::File {
                        file: WireFile {
                            filename: name,
                            file_data: DataUrl::new(mime, bytes.unwrap_or_default()),
                        },
                    },
                });
            }
            Some(WireContent::Parts(wire))
        } else {
            message.content().map(WireContent::Text)
        };
        Ok(Self {
            role: message.role(),
            content,
            tool_calls: message.tool_calls(),
            tool_call_id: message.tool_call_id(),
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiTool<'a> {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: &'a ModelTool,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    error: Option<ChunkError>,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<WireChatUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireChatUsage<CacheWrite = serde::de::IgnoredAny> {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptTokensDetails<CacheWrite>>,
    #[serde(default)]
    completion_tokens_details: Option<WireCompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct WirePromptTokensDetails<CacheWrite> {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<CacheWrite>,
}

#[derive(Debug, Deserialize)]
struct WireCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl<CacheWrite> From<WireChatUsage<CacheWrite>> for ModelUsage {
    fn from(usage: WireChatUsage<CacheWrite>) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            cache_write_tokens: None,
            cached_input_tokens: usage
                .prompt_tokens_details
                .and_then(|details| details.cached_tokens),
            output_tokens: usage.completion_tokens,
            reasoning_output_tokens: usage
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens),
            total_tokens: usage.total_tokens,
        }
    }
}

impl WireChatUsage<u64> {
    pub(crate) fn into_openrouter(self) -> ModelUsage {
        let cache_write_tokens = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cache_write_tokens);
        ModelUsage {
            cache_write_tokens,
            ..self.into()
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    finish_reason: Option<FinishReason>,
    message: WireAssistantMessage,
}

#[derive(Debug, Deserialize)]
struct WireAssistantMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolCall>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunctionCall,
}

#[derive(Debug, Deserialize)]
struct WireFunctionCall {
    name: String,
    arguments: Value,
}

impl WireToolCall {
    pub(crate) fn into_model(
        self,
        secrets: crate::diagnostic::DiagnosticSecrets<'_>,
    ) -> Result<ModelToolCall, InferenceError> {
        let call = self;
        if call.kind != "function" {
            return Err(UnsupportedFeature::ToolKind(secrets.sanitize(&call.kind)).into());
        }
        let arguments = match call.function.arguments {
            Value::String(arguments) => arguments,
            arguments @ Value::Object(_) => {
                serde_json::to_string(&arguments).map_err(ProtocolFailure::Decode)?
            }
            _ => {
                return Err(ProtocolFailure::InvalidToolArguments {
                    name: secrets.sanitize(&call.function.name),
                }
                .into());
            }
        };

        if call.id.is_empty() || call.function.name.is_empty() {
            return Err(ProtocolFailure::IncompleteToolCall.into());
        }
        let parsed: Value =
            serde_json::from_str(&arguments).map_err(|error| secrets.decode_failure(error))?;
        if !parsed.is_object() {
            return Err(ProtocolFailure::InvalidToolArguments {
                name: secrets.sanitize(&call.function.name),
            }
            .into());
        }
        Ok(ModelToolCall {
            id: call.id.into(),
            kind: call.kind,
            function: ModelFunctionCall {
                name: call.function.name,
                arguments,
            },
        })
    }
}

pub(crate) fn replay_transcript(
    secrets: DiagnosticSecrets<'_>,
    body: &str,
    on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
) -> Result<AssistantTurn, InferenceError> {
    let mut state = ChatStream::default();
    decode_transcript(body, &mut |event| match event {
        SseEvent::Done => {
            state.finished = true;
            Ok(ControlFlow::Break(()))
        }
        SseEvent::Data(data) => {
            let chunk =
                serde_json::from_str(data).map_err(|error| secrets.decode_failure(error))?;
            if state.apply(chunk, on_event, secrets)?.is_break() {
                return Err(InferenceError::Cancelled);
            }
            Ok(ControlFlow::Continue(()))
        }
    })?;
    if !state.finished {
        return Err(ProtocolFailure::MissingTerminal.into());
    }
    state.into_turn(secrets)
}

/// Every field is optional and every absence tolerated, since being OpenAI-compatible is a claim,
/// not a spec: llama.cpp, Ollama, vLLM, and other proxies each omit or null a different one.
#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<WireChatUsage>,
    #[serde(default)]
    error: Option<ChunkError>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkError {
    #[serde(default)]
    code: Option<crate::error::ProviderCode>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Option<ChunkDelta>,
    #[serde(default)]
    finish_reason: Option<FinishReason>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ChunkToolCall {
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    function: Option<ChunkFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug)]
struct StreamedCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct ChatStream {
    content: String,
    calls: Vec<StreamedCall>,
    latest: HashMap<u64, usize>,
    usage: Option<ModelUsage>,
    finished: bool,
    choices_seen: bool,
    finish_reason: Option<FinishReason>,
}

impl ChatStream {
    fn apply(
        &mut self,
        chunk: ChatChunk,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        secrets: crate::diagnostic::DiagnosticSecrets<'_>,
    ) -> Result<ControlFlow<()>, InferenceError> {
        record_model(chunk.model.as_deref(), secrets);
        if let Some(error) = chunk.error {
            return Err(stream_failure(error, secrets));
        }
        // Overwrite, not accumulate: some endpoints send one final usage chunk, others repeat a
        // running total each chunk.
        if let Some(usage) = chunk.usage {
            self.usage = Some(ModelUsage::from(usage));
        }
        for choice in chunk.choices {
            self.choices_seen = true;
            if let Some(reason) = choice.finish_reason {
                reason.record(secrets)?;
                self.finished = true;
                self.finish_reason = Some(reason);
            }
            let delta = choice.delta.unwrap_or_default();
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                self.content.push_str(&text);
                if on_event(TurnEvent::TextDelta(ModelText::from_model(text))).is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            }
            for fragment in delta.tool_calls.unwrap_or_default() {
                if let Some(index) = self.merge_call(fragment)
                    && on_event(TurnEvent::ToolCallStarted { index }).is_break()
                {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn merge_call(&mut self, fragment: ChunkToolCall) -> Option<u32> {
        let index = fragment.index.unwrap_or(0);
        let function = fragment.function.unwrap_or_default();
        let name = function.name.filter(|name| !name.is_empty());
        let arguments = function.arguments.unwrap_or_default();

        let slot = match self.latest.get(&index).copied() {
            // Ollama reports every parallel call at index 0, so a fragment naming a function when
            // that index already has one starts a new call rather than continuing it.
            Some(position) if name.is_some() && !self.calls[position].name.is_empty() => None,
            other => other,
        };
        let Some(position) = slot else {
            self.calls.push(StreamedCall {
                id: fragment.id.unwrap_or_default(),
                kind: fragment.kind.unwrap_or_default(),
                name: name.unwrap_or_default(),
                arguments,
            });
            self.latest.insert(index, self.calls.len() - 1);
            return Some(u32::try_from(self.calls.len() - 1).unwrap_or(u32::MAX));
        };
        let call = &mut self.calls[position];
        if let Some(id) = fragment.id.filter(|id| !id.is_empty()) {
            call.id = id;
        }
        if let Some(kind) = fragment.kind.filter(|kind| !kind.is_empty()) {
            call.kind = kind;
        }
        if let Some(name) = name {
            call.name = name;
        }
        // llama.cpp has answered with the whole argument document in one fragment and OpenAI sends
        // it a few characters at a time; appending is the same operation for both.
        call.arguments.push_str(&arguments);
        None
    }

    fn into_turn(
        self,
        secrets: crate::diagnostic::DiagnosticSecrets<'_>,
    ) -> Result<AssistantTurn, InferenceError> {
        if !self.choices_seen {
            return Err(ProtocolFailure::NoChoices.into());
        }
        let tool_calls = self
            .calls
            .into_iter()
            .map(|call| {
                WireToolCall {
                    id: call.id,
                    kind: if call.kind.is_empty() {
                        "function".to_owned()
                    } else {
                        call.kind
                    },
                    function: WireFunctionCall {
                        name: call.name,
                        arguments: Value::String(call.arguments),
                    },
                }
                .into_model(secrets)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(complete_turn(
            (!self.content.is_empty()).then_some(self.content),
            tool_calls,
            self.usage,
            self.finish_reason.as_ref(),
        ))
    }
}

/// Must derive the host exactly as the transport does: Uri::host ignores userinfo, so an authority
/// like 127.0.0.1:80@evil.test actually connects to evil.test despite looking like loopback.
fn allows_bearer_token(endpoint: &str) -> bool {
    let Ok(uri) = endpoint.parse::<http::Uri>() else {
        return false;
    };
    let Some(scheme) = uri.scheme_str() else {
        return false;
    };
    if scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    scheme.eq_ignore_ascii_case("http") && uri.host().is_some_and(is_loopback_host)
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|literal| literal.strip_suffix(']'))
        .unwrap_or(host)
        .to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
}

fn completion_url(endpoint: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    if endpoint.ends_with("/chat/completions") {
        endpoint.to_owned()
    } else {
        format!("{endpoint}/chat/completions")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::time::Duration;

    use serde_json::Value;

    use super::{
        AssistantTurn, ChatRequest, ChatResponse, ContentPart, ControlFlow, InferenceError,
        ModelFunctionCall, ModelMessage, ModelTool, ModelToolCall, ModelUsage, OpenAiClient,
        OpenAiTool, StreamOptions, TurnEvent, WireFunctionCall, WireMessage, WireToolCall,
        completion_url, replay_transcript, turn_from_response,
    };
    use crate::error::{AuthError, RateLimitError};
    use crate::mock::{MockResponse, MockServer};
    use crate::model::{ChatModel, ClientIdentity, CompletionOptions, assistant_message};
    use crate::{
        control::TurnControl,
        inference::{GenerateRequest, InferenceModel},
    };
    use tracing::instrument::WithSubscriber as _;

    async fn generate(
        model: &OpenAiClient,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError> {
        let control =
            TurnControl::new(tokio::sync::watch::channel(false).1, Duration::from_secs(2))?;
        model
            .generate(
                GenerateRequest {
                    messages,
                    tools,
                    options,
                },
                observe,
                &control,
            )
            .await
    }

    async fn wire_messages(messages: &[ModelMessage]) -> Vec<WireMessage<'_>> {
        let mut wire = Vec::new();
        for message in messages {
            wire.push(WireMessage::prepare(message).await.unwrap());
        }
        wire
    }

    #[tokio::test]
    async fn compatible_cancellation_interrupts_silent_streamed_and_buffered_bodies() {
        for stream in [true, false] {
            let reply = if stream {
                MockResponse::sse("data: [DONE]\n\n")
            } else {
                MockResponse::json(json!({"choices":[]}))
            };
            let (reply, ready, release) = reply.stalled();
            let server = MockServer::start(vec![reply]);
            let client =
                OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2))
                    .unwrap()
                    .with_streaming(stream);
            let (cancel, watch) = tokio::sync::watch::channel(false);
            let control = TurnControl::new(watch, Duration::from_secs(2)).unwrap();
            let options = CompletionOptions::default();
            let request = GenerateRequest {
                messages: &[],
                tools: &[],
                options: &options,
            };
            let mut observe = ignored;
            let (result, ()) =
                tokio::join!(client.generate(request, &mut observe, &control), async {
                    ready.await.unwrap();
                    cancel.send(true).unwrap();
                });
            drop(release);
            assert!(matches!(result, Err(InferenceError::Cancelled)));
            assert_eq!(server.requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn compatible_deadlines_interrupt_silent_streamed_and_buffered_bodies_without_retry() {
        for stream in [true, false] {
            let reply = if stream {
                MockResponse::sse("data: [DONE]\n\n")
            } else {
                MockResponse::json(json!({"choices":[]}))
            };
            let (reply, _ready, release) = reply.header("x-request-id", "req-deadline").stalled();
            let server = MockServer::start(vec![reply]);
            let trace = crate::trace_capture::TraceCapture::default();
            let client =
                OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2))
                    .unwrap()
                    .with_streaming(stream);
            let control =
                TurnControl::new(tokio::sync::watch::channel(false).1, Duration::from_secs(1))
                    .unwrap();
            let result = client
                .generate(
                    GenerateRequest {
                        messages: &[],
                        tools: &[],
                        options: &CompletionOptions::default(),
                    },
                    &mut ignored,
                    &control,
                )
                .with_subscriber(trace.subscriber())
                .await;
            drop(release);
            assert!(matches!(result, Err(InferenceError::DeadlineExceeded)));
            assert_eq!(server.requests().len(), 1);
            let fields = trace.text();
            assert!(fields.contains("http.status=200"), "{fields}");
            assert!(
                fields.contains("provider.request_id=\"req-deadline\""),
                "{fields}"
            );
            assert!(fields.contains("error.phase=\"reading-body\""), "{fields}");
        }
    }

    #[tokio::test]
    async fn compatible_trace_records_streamed_and_buffered_usage_without_credentials() {
        use tracing::instrument::WithSubscriber as _;
        for stream in [true, false] {
            for reported_usage in [true, false] {
                let mut body = json!({"model":"reported-model","choices":[{"message":{"content":"answer"},"delta":{"content":"answer"},"finish_reason":"stop"}]});
                if reported_usage {
                    body["usage"] = json!({"prompt_tokens":11,"completion_tokens":4,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":"ignored-compatible-extension"},"completion_tokens_details":{"reasoning_tokens":2}});
                }
                let reply = if stream {
                    MockResponse::sse(&format!("data: {body}\n\ndata: [DONE]\n\n"))
                } else {
                    MockResponse::json(body)
                };
                let server = MockServer::start(vec![reply]);
                let client = OpenAiClient::new(
                    server.base_url(),
                    "requested-model",
                    Some("trace-credential-sentinel".into()),
                    Duration::from_secs(2),
                )
                .unwrap()
                .with_streaming(stream)
                .with_name("compatible-trace");
                let capture = crate::trace_capture::TraceCapture::default();
                generate(
                    &client,
                    &[],
                    &[],
                    &CompletionOptions::default().with_prompt_cache_key("affinity-sentinel"),
                    &mut ignored,
                )
                .with_subscriber(capture.subscriber())
                .await
                .unwrap();
                capture.assert_exchange("openai-compatible", "chat-completions");
                assert_eq!(
                    capture.field("model.name").as_deref(),
                    Some("\"compatible-trace\"")
                );
                assert_eq!(
                    capture.field("model.returned").as_deref(),
                    Some("\"reported-model\"")
                );
                assert_eq!(capture.field("model.stream"), Some(stream.to_string()));
                assert_eq!(capture.field("timing.first_event_ms").is_some(), stream);
                assert_eq!(capture.field("stream.first_delta_ms").is_some(), stream);
                assert_eq!(capture.field("finish.reason").as_deref(), Some("\"stop\""));
                assert_eq!(capture.field("output.partial").as_deref(), Some("false"));
                if reported_usage {
                    for (field, value) in [
                        ("input_tokens", 11),
                        ("output_tokens", 4),
                        ("total_tokens", 15),
                        ("cached_input_tokens", 3),
                        ("reasoning_output_tokens", 2),
                    ] {
                        assert_eq!(
                            capture.field(&format!("usage.{field}")),
                            Some(value.to_string())
                        );
                    }
                } else {
                    assert!(!capture.text().contains("usage."));
                }
                assert!(capture.field("usage.cache_write_tokens").is_none());
                for secret in ["trace-credential-sentinel", "affinity-sentinel"] {
                    assert!(!capture.text().contains(secret));
                }
            }
        }
    }

    async fn buffered_at_size(bytes: usize) -> Result<AssistantTurn, InferenceError> {
        let empty = json!({"choices":[{"message":{"content":""}}]});
        let body = json!({"choices":[{"message":{"content":"x".repeat(bytes - empty.to_string().len())}}]});
        assert_eq!(body.to_string().len(), bytes);
        let server = MockServer::start(vec![MockResponse::json(body)]);
        let client = OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(5))
            .unwrap()
            .with_streaming(false);
        generate(
            &client,
            &[],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
    }

    #[tokio::test]
    async fn a_buffered_response_one_byte_over_the_existing_ceiling_is_refused() {
        assert!(matches!(
            buffered_at_size(crate::http::MAX_BUFFERED_BYTES + 1).await,
            Err(InferenceError::Protocol(
                crate::error::ProtocolFailure::BufferedTooLarge
            ))
        ));
    }

    #[tokio::test]
    async fn both_modes_preserve_error_classes_metadata_and_single_request_counts() {
        for stream in [true, false] {
            for (status, retry) in [
                (401, None),
                (403, None),
                (429, Some(Duration::from_secs(7))),
                (500, None),
            ] {
                let response = MockResponse::failure(
                    status,
                    json!({"error":{"message":"refused", "code":"quota"}}),
                )
                .header("x-request-id", "req-1");
                let response = if status == 429 {
                    response.header("retry-after", "7")
                } else {
                    response
                };
                let server = MockServer::start(vec![response]);
                let client =
                    OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2))
                        .unwrap()
                        .with_streaming(stream);
                let error = generate(
                    &client,
                    &[],
                    &[],
                    &CompletionOptions::default(),
                    &mut ignored,
                )
                .await
                .unwrap_err();
                let context = match (status, error) {
                    (401 | 403, InferenceError::Authentication(AuthError::Provider(context))) => {
                        context
                    }
                    (429, InferenceError::RateLimited(RateLimitError(context))) => context,
                    (500, InferenceError::Provider(context)) => context,
                    _ => panic!("incorrect HTTP classification"),
                };
                assert_eq!(context.status, Some(status));
                assert_eq!(context.retry_after, retry);
                assert_eq!(context.request_id.as_deref(), Some("req-1"));
                assert_eq!(context.code.as_deref(), Some("quota"));
                assert_eq!(server.requests().len(), 1);
            }
        }
    }

    #[tokio::test]
    async fn a_body_io_failure_retains_its_transport_source_and_safe_request_id_in_both_modes() {
        for stream in [true, false] {
            let body = if stream {
                MockResponse::sse("data: {}\n\n")
            } else {
                MockResponse::json(json!({"choices":[]}))
            };
            let server = MockServer::start(vec![
                body.interrupted_at(1).header("x-request-id", "req-io"),
            ]);
            let client =
                OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2))
                    .unwrap()
                    .with_streaming(stream);
            let error = generate(
                &client,
                &[],
                &[],
                &CompletionOptions::default(),
                &mut ignored,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(&error, InferenceError::Transport(crate::error::TransportFailure::Http { context, source }) if context.status == Some(200) && context.request_id.as_deref() == Some("req-io") && source.is_decode()),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn buffered_and_streamed_200_errors_keep_codes_and_redact_credentials() {
        let token = "synthetic-compatible-secret";
        for stream in [true, false] {
            let body = json!({"model":token, "error":{"code":token, "message":token}});
            let response = if stream {
                MockResponse::sse(&format!("data: {body}\n\n"))
            } else {
                MockResponse::json(body)
            };
            let server = MockServer::start(vec![response.header("x-request-id", token)]);
            let client = OpenAiClient::new(
                server.base_url(),
                "fixture",
                Some(token.to_owned()),
                Duration::from_secs(2),
            )
            .unwrap()
            .with_streaming(stream);
            let trace = crate::trace_capture::TraceCapture::default();
            let error = generate(
                &client,
                &[],
                &[],
                &CompletionOptions::default(),
                &mut ignored,
            )
            .with_subscriber(trace.subscriber())
            .await
            .unwrap_err();
            assert!(
                matches!(&error, InferenceError::Provider(context) if context.status == Some(200) && context.code.as_deref() == Some("[REDACTED]") && context.request_id.as_deref() == Some("[REDACTED]"))
            );
            assert!(!format!("{error} {error:?} {}", trace.text()).contains(token));
        }
    }

    #[tokio::test]
    async fn successful_text_finish_reasons_and_complete_calls_agree_across_modes() {
        for reason in ["stop", "length", "provider_extension"] {
            let buffered =
                json!({"choices":[{"message":{"content":"answer"}, "finish_reason":reason}]});
            let streamed = format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"content":"answer"}, "finish_reason":reason}]})
            );
            let expected = turn_from_response(
                crate::diagnostic::DiagnosticSecrets::default(),
                serde_json::from_value(buffered).unwrap(),
            )
            .unwrap();
            let actual = replay_transcript(
                crate::diagnostic::DiagnosticSecrets::default(),
                &streamed,
                &mut ignored,
            )
            .unwrap();
            assert_eq!(expected, actual);
            assert_eq!(actual.content.as_deref(), Some("answer"));
        }
    }

    #[tokio::test]
    async fn filtered_responses_and_incomplete_tool_arguments_never_become_turns_in_either_mode() {
        for stream in [true, false] {
            for (reason, arguments) in [
                ("content_filter", "{}"),
                ("length", "{"),
                ("tool_calls", "{"),
            ] {
                let call = json!({"index":0,"id":"call-1", "type":"function", "function":{"name":"tool","arguments":arguments}});
                let chunk = json!({"choices":[{"delta":{"tool_calls":[call.clone()]},"finish_reason":reason}]});
                let body =
                    json!({"choices":[{"message":{"tool_calls":[call]},"finish_reason":reason}]});
                let reply = if stream {
                    MockResponse::sse(&format!("data: {chunk}\n\n"))
                } else {
                    MockResponse::json(body)
                };
                let server = MockServer::start(vec![reply]);
                let client =
                    OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2))
                        .unwrap()
                        .with_streaming(stream);
                let error = generate(
                    &client,
                    &[],
                    &[],
                    &CompletionOptions::default(),
                    &mut ignored,
                )
                .await
                .unwrap_err();
                if reason == "content_filter" {
                    assert!(
                        matches!(error, InferenceError::Provider(context) if context.status == Some(200))
                    );
                } else {
                    assert!(matches!(
                        error,
                        InferenceError::Protocol(crate::error::ProtocolFailure::Decode(_))
                    ));
                }
            }
        }
    }

    #[tokio::test]
    async fn malformed_buffered_json_and_choice_free_streams_are_protocol_failures() {
        for (stream, reply) in [
            (false, MockResponse::sse("{")),
            (true, MockResponse::sse("data: [DONE]\n\n")),
        ] {
            let server = MockServer::start(vec![reply]);
            let client =
                OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2))
                    .unwrap()
                    .with_streaming(stream);
            let error = generate(
                &client,
                &[],
                &[],
                &CompletionOptions::default(),
                &mut ignored,
            )
            .await
            .unwrap_err();
            assert!(matches!(
                error,
                InferenceError::Protocol(
                    crate::error::ProtocolFailure::Decode(_)
                        | crate::error::ProtocolFailure::NoChoices
                )
            ));
        }
    }

    #[tokio::test]
    async fn length_with_complete_calls_succeeds_but_missing_call_identity_and_nonobjects_fail() {
        for stream in [true, false] {
            for (id, name, arguments) in [
                ("call", "tool", "{}"),
                ("", "tool", "{}"),
                ("call", "", "{}"),
                ("call", "tool", "[]"),
            ] {
                let call = json!({"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments}});
                let body = if stream {
                    json!({"choices":[{"delta":{"tool_calls":[call]},"finish_reason":"length"}]})
                } else {
                    json!({"choices":[{"message":{"tool_calls":[call]},"finish_reason":"length"}]})
                };
                let reply = if stream {
                    MockResponse::sse(&format!("data: {body}\n\n"))
                } else {
                    MockResponse::json(body)
                };
                let server = MockServer::start(vec![reply]);
                let client =
                    OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2))
                        .unwrap()
                        .with_streaming(stream);
                let result = generate(
                    &client,
                    &[],
                    &[],
                    &CompletionOptions::default(),
                    &mut ignored,
                )
                .await;
                if id.is_empty() || name.is_empty() {
                    assert!(matches!(
                        result,
                        Err(InferenceError::Protocol(
                            crate::error::ProtocolFailure::IncompleteToolCall
                        ))
                    ));
                } else if arguments == "[]" {
                    assert!(matches!(
                        result,
                        Err(InferenceError::Protocol(
                            crate::error::ProtocolFailure::InvalidToolArguments { .. }
                        ))
                    ));
                } else {
                    assert_eq!(result.unwrap().tool_calls.len(), 1);
                }
            }
        }
    }

    #[tokio::test]
    async fn a_foreign_native_continuation_is_refused_before_sending() {
        let server = MockServer::start(Vec::new());
        let client =
            OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2)).unwrap();
        let turn = AssistantTurn::new(Some("portable projection".into()), Vec::new(), None)
            .with_codex_continuation(Vec::new(), ClientIdentity::new(), None);
        let error = generate(
            &client,
            &[assistant_message(&turn)],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            InferenceError::Protocol(crate::error::ProtocolFailure::ContinuationMismatch)
        ));
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn compatible_stream_chunks_can_split_utf8_and_json_without_changing_the_turn() {
        let body = frames(&[
            r#"{"choices":[{"delta":{"content":"🍊"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        let server = MockServer::start(vec![MockResponse::sse(&body).split(1)]);
        let client =
            OpenAiClient::new(server.base_url(), "fixture", None, Duration::from_secs(2)).unwrap();
        let turn = generate(
            &client,
            &[],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .unwrap();
        assert_eq!(turn.content.as_deref(), Some("🍊"));
    }

    #[tokio::test]
    async fn protocol_failures_keep_known_sanitized_headers_and_body_phase() {
        let token = "synthetic-header-secret";
        for (stream, reply) in [
            (false, MockResponse::sse("{")),
            (true, MockResponse::sse("data: {\n\n")),
            (
                true,
                MockResponse::sse(
                    r#"data: {"choices":[{"delta":{"content":"partial"}}]}

"#,
                ),
            ),
            (
                true,
                MockResponse::sse(
                    r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"tool","arguments":"{"}}]},"finish_reason":"tool_calls"}]}

"#,
                ),
            ),
        ] {
            let server = MockServer::start(vec![reply.header("x-request-id", token)]);
            let client = OpenAiClient::new(
                server.base_url(),
                "fixture",
                Some(token.to_owned()),
                Duration::from_secs(2),
            )
            .unwrap()
            .with_streaming(stream);
            let trace = crate::trace_capture::TraceCapture::default();
            let error = generate(
                &client,
                &[],
                &[],
                &CompletionOptions::default(),
                &mut ignored,
            )
            .with_subscriber(trace.subscriber())
            .await
            .unwrap_err();
            assert!(matches!(error, InferenceError::Protocol(_)));
            let fields = trace.text();
            assert!(fields.contains("http.status=200"), "{fields}");
            assert!(
                fields.contains("provider.request_id=\"[REDACTED]\""),
                "{fields}"
            );
            assert!(fields.contains("error.phase=\"reading-body\""), "{fields}");
            assert!(!fields.contains(token));
        }
    }

    #[tokio::test]
    async fn cancellation_after_visible_output_keeps_headers_and_body_phase() {
        let token = "synthetic-cancel-secret";
        let body = frames(&[r#"{"choices":[{"delta":{"content":"partial"}}]}"#, "[DONE]"]);
        let server =
            MockServer::start(vec![MockResponse::sse(&body).header("x-request-id", token)]);
        let client = OpenAiClient::new(
            server.base_url(),
            "fixture",
            Some(token.to_owned()),
            Duration::from_secs(2),
        )
        .unwrap();
        let (cancel, watch) = tokio::sync::watch::channel(false);
        let control = TurnControl::new(watch, Duration::from_secs(2)).unwrap();
        let trace = crate::trace_capture::TraceCapture::default();
        let mut observe = |_| {
            cancel.send(true).unwrap();
            ControlFlow::Continue(())
        };
        let result = client
            .generate(
                GenerateRequest {
                    messages: &[],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut observe,
                &control,
            )
            .with_subscriber(trace.subscriber())
            .await;
        assert!(matches!(result, Err(InferenceError::Cancelled)));
        let fields = trace.text();
        assert!(fields.contains("http.status=200"), "{fields}");
        assert!(
            fields.contains("provider.request_id=\"[REDACTED]\""),
            "{fields}"
        );
        assert!(fields.contains("error.phase=\"reading-body\""), "{fields}");
        assert!(fields.contains("output.partial=true"), "{fields}");
        assert!(!fields.contains(token));
    }

    fn ignored(_event: TurnEvent) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn recorded(events: &[TurnEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                TurnEvent::TextDelta(text) => format!("text:{}", text.as_str()),
                TurnEvent::ToolCallStarted { index } => format!("call:{index}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_failed_completion_reports_the_endpoints_own_error_body() {
        let server = MockServer::start(vec![MockResponse::failure(
            429,
            json!({"error": {"message": "Rate limit reached for gpt-test", "type": "rate_limit"}}),
        )]);
        let model = OpenAiClient::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
            .expect("model client");

        let error = generate(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect_err("a 429 must fail the turn");

        assert!(
            matches!(&error, InferenceError::RateLimited(RateLimitError(failure))
            if failure.status == Some(429)
                && failure.diagnostic.contains("Rate limit reached for gpt-test"))
        );
    }

    #[tokio::test]
    async fn an_endpoint_that_ignores_stream_true_is_refused_naming_the_key_to_write() {
        let server = MockServer::start(vec![MockResponse::json(
            json!({"choices": [{"message": {"role": "assistant", "content": "hello"}}]}),
        )]);
        let model = OpenAiClient::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
            .expect("model client");
        assert!(
            model.stream,
            "streaming is the default for a compatible endpoint"
        );

        let error = generate(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect_err("a JSON answer to a streamed request fails the turn");

        assert!(matches!(error,
            InferenceError::Protocol(crate::error::ProtocolFailure::UnexpectedContentType(content_type))
                if content_type == "`application/json`"
        ));
    }

    #[tokio::test]
    async fn appends_chat_completions_to_api_bases() {
        assert_eq!(
            completion_url("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            completion_url("https://example.test/chat/completions"),
            "https://example.test/chat/completions"
        );
    }

    #[tokio::test]
    async fn compatible_refusals_never_retain_the_requests_bearer_credential() {
        let token = "synthetic-compatible-secret";
        for response in [
            MockResponse::failure(401, json!({"error":{"message":token}})),
            MockResponse::failure(403, json!({"error":{"message":token}})),
            MockResponse::sse(&format!("data: {}\n\n", json!({"error":{"message":token}}))),
        ] {
            let server = MockServer::start(vec![response]);
            let model = OpenAiClient::new(
                server.base_url(),
                "fixture",
                Some(token.to_owned()),
                Duration::from_secs(2),
            )
            .unwrap();
            let trace = crate::trace_capture::TraceCapture::default();
            let error = generate(
                &model,
                &[ModelMessage::user("test")],
                &[],
                &CompletionOptions::default(),
                &mut ignored,
            )
            .with_subscriber(trace.subscriber())
            .await
            .unwrap_err();
            assert!(
                matches!(&error, InferenceError::Authentication(AuthError::Provider(context)) if matches!(context.status, Some(401 | 403)))
                    || matches!(&error, InferenceError::Provider(context) if context.status == Some(200))
            );
            let shown = format!("{error}\n{error:?}\n{}", trace.text());
            assert!(shown.contains("[REDACTED]"));
            assert!(!shown.contains(token));
            assert!(trace.text().contains("model.complete"));
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn refuses_bearer_tokens_over_remote_plaintext_http() {
        let error = OpenAiClient::new(
            "http://models.example.test/v1",
            "test-model",
            Some("secret".to_owned()),
            Duration::from_secs(1),
        )
        .err()
        .expect("remote bearer tokens require TLS");

        assert!(matches!(error, InferenceError::InvalidRequest(_)));

        for disguised in [
            "http://127.0.0.1:80@models.example.test/v1",
            "http://localhost@models.example.test/v1",
            "http://[::1]@models.example.test/v1",
        ] {
            let error = OpenAiClient::new(
                disguised,
                "test-model",
                Some("secret".to_owned()),
                Duration::from_secs(1),
            )
            .err()
            .unwrap_or_else(|| panic!("{disguised} connects to a remote host in plaintext"));
            assert!(matches!(error, InferenceError::InvalidRequest(_)));
        }

        for loopback in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:11434/v1",
            "http://[::1]:11434/v1",
        ] {
            assert!(
                OpenAiClient::new(
                    loopback,
                    "test-model",
                    Some("local-secret".to_owned()),
                    Duration::from_secs(1),
                )
                .is_ok(),
                "{loopback} is a loopback endpoint"
            );
        }
        assert!(
            OpenAiClient::new(
                "https://models.example.test/v1",
                "test-model",
                Some("secret".to_owned()),
                Duration::from_secs(1),
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn normalizes_chat_completion_usage() {
        let response: ChatResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": "hi"}}],
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 30,
                "total_tokens": 150,
                "prompt_tokens_details": {"cached_tokens": 100, "audio_tokens": 0},
                "completion_tokens_details": {"reasoning_tokens": 7}
            }
        }))
        .expect("usage-bearing response deserializes");

        assert_eq!(
            ModelUsage::from(response.usage.expect("usage present")),
            ModelUsage {
                input_tokens: Some(120),
                cache_write_tokens: None,
                cached_input_tokens: Some(100),
                output_tokens: Some(30),
                reasoning_output_tokens: Some(7),
                total_tokens: Some(150),
            }
        );
    }

    #[tokio::test]
    async fn keeps_bare_usage_counts_from_minimal_endpoints() {
        let response: ChatResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
        }))
        .expect("bare usage deserializes");

        assert_eq!(
            ModelUsage::from(response.usage.expect("usage present")),
            ModelUsage {
                input_tokens: Some(8),
                cache_write_tokens: None,
                cached_input_tokens: None,
                output_tokens: Some(2),
                reasoning_output_tokens: None,
                total_tokens: Some(10),
            }
        );
    }

    #[tokio::test]
    async fn accepts_object_arguments_from_compatible_endpoints() {
        let call = WireToolCall {
            id: "call-1".to_owned(),
            kind: "function".to_owned(),
            function: WireFunctionCall {
                name: "echo_echo".to_owned(),
                arguments: json!({"message": "hi"}),
            },
        }
        .into_model(crate::diagnostic::DiagnosticSecrets::default())
        .expect("object arguments normalize");

        assert_eq!(call.function.arguments, r#"{"message":"hi"}"#);
    }

    fn request_text(fragment: &Value) -> String {
        serde_json::to_string(fragment).expect("serialize request fragment")
    }

    #[tokio::test]
    async fn an_appended_turn_extends_the_chat_request_without_disturbing_its_prefix() {
        let tool = ModelTool {
            name: "bash".to_owned(),
            description: "Run a sandboxed script".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"script": {"type": "string"}},
                "required": ["script"],
            }),
        };
        let tools = vec![OpenAiTool {
            kind: "function",
            function: &tool,
        }];
        let mut messages = vec![
            ModelMessage::system("Be concise."),
            ModelMessage::user("how many files are in the repository?"),
        ];
        let mut bodies = Vec::new();
        for turn in 1..=3_u32 {
            bodies.push(
                serde_json::to_value(ChatRequest {
                    model: "test-model",
                    messages: &wire_messages(&messages).await,
                    tools: &tools,
                    tool_choice: "auto",
                    prompt_cache_key: None,
                    stream: Some(true),
                    stream_options: Some(StreamOptions {
                        include_usage: true,
                    }),
                })
                .expect("serialize chat request"),
            );
            let call_id = format!("call_{turn}");
            messages.push(assistant_message(&AssistantTurn::new(
                None,
                vec![ModelToolCall {
                    id: call_id.clone().into(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: "bash".to_owned(),
                        arguments: r#"{"script":"ls | wc -l"}"#.to_owned(),
                    },
                }],
                None,
            )));
            messages.push(ModelMessage::tool(call_id, "12\n"));
        }

        assert_eq!(bodies[0]["messages"][0]["role"], "system");
        for pair in bodies.windows(2) {
            let (previous, next) = (&pair[0], &pair[1]);
            assert_eq!(
                request_text(&previous["tools"]),
                request_text(&next["tools"]),
                "an appended turn rewrote the tool definitions"
            );
            let previous_messages = previous["messages"].as_array().expect("messages array");
            let next_messages = next["messages"].as_array().expect("messages array");
            assert!(
                next_messages.len() > previous_messages.len(),
                "an appended turn must extend the messages rather than replace them"
            );
            for (index, message) in previous_messages.iter().enumerate() {
                assert_eq!(
                    request_text(message),
                    request_text(&next_messages[index]),
                    "message {index} changed between turns; the cached prefix ends there"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_chat_request_carries_a_cache_key_only_when_one_is_set() {
        let tool = ModelTool {
            name: "bash".to_owned(),
            description: "Run a sandboxed script".to_owned(),
            parameters: json!({"type": "object"}),
        };
        let tools = vec![OpenAiTool {
            kind: "function",
            function: &tool,
        }];
        let messages = [
            ModelMessage::system("Be concise."),
            ModelMessage::user("how many files are in the repository?"),
        ];
        let wire = wire_messages(&messages).await;
        let request = |prompt_cache_key| {
            serde_json::to_value(ChatRequest {
                model: "test-model",
                messages: &wire,
                tools: &tools,
                tool_choice: "auto",
                prompt_cache_key,
                stream: Some(true),
                stream_options: Some(StreamOptions {
                    include_usage: true,
                }),
            })
            .expect("serialize chat request")
        };

        let plain = request(None);
        let keyed = request(Some("session-7"));

        assert!(
            plain.get("prompt_cache_key").is_none(),
            "a keyless request grew a cache field"
        );
        assert!(!request_text(&plain).contains("prompt_cache_key"));
        assert_eq!(keyed["prompt_cache_key"], "session-7");
        let plain_fields = plain.as_object().expect("request object");
        let keyed_fields = keyed.as_object().expect("request object");
        assert_eq!(
            keyed_fields.len(),
            plain_fields.len() + 1,
            "the cache key added or removed a field other than its own"
        );
        for (field, value) in plain_fields {
            assert_eq!(
                request_text(value),
                request_text(&keyed_fields[field]),
                "the cache key rewrote {field}, which is part of the prefix it is supposed to hit"
            );
        }
    }

    #[tokio::test]
    async fn a_blank_cache_key_is_dropped_rather_than_routed() {
        assert_eq!(CompletionOptions::default().prompt_cache_key(), None);
        assert_eq!(
            CompletionOptions::default()
                .with_prompt_cache_key(" \t\n")
                .prompt_cache_key(),
            None,
            "a caller that computed an empty identifier must send no key at all"
        );
        assert_eq!(
            CompletionOptions::default()
                .with_prompt_cache_key("session-7")
                .prompt_cache_key(),
            Some("session-7")
        );
    }

    #[tokio::test]
    async fn an_implementation_that_cannot_stream_answers_without_reporting_an_event() {
        struct WholeTurnModel;

        impl ChatModel for WholeTurnModel {
            fn complete(
                &self,
                messages: &[ModelMessage],
                _tools: &[ModelTool],
                _options: &CompletionOptions,
                _on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
            ) -> Result<AssistantTurn, InferenceError> {
                Ok(AssistantTurn::new(
                    messages.last().and_then(|message| {
                        message.content().map(|content| content.to_uppercase())
                    }),
                    Vec::new(),
                    None,
                ))
            }
        }

        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Continue(())
        };

        let turn = WholeTurnModel
            .complete(
                &[ModelMessage::user("hello")],
                &[],
                &CompletionOptions::default().with_prompt_cache_key("session-7"),
                &mut sink,
            )
            .expect("a model that cannot stream still answers");

        assert_eq!(turn.content.as_deref(), Some("HELLO"));
        assert!(recorded(&events).is_empty());
    }

    #[tokio::test]
    async fn rejects_non_function_tool_calls() {
        let error = WireToolCall {
            id: "call-1".to_owned(),
            kind: "computer".to_owned(),
            function: WireFunctionCall {
                name: "click".to_owned(),
                arguments: json!({}),
            },
        }
        .into_model(crate::diagnostic::DiagnosticSecrets::default())
        .expect_err("only function tools are supported");

        assert!(
            matches!(error, InferenceError::Unsupported(crate::error::UnsupportedFeature::ToolKind(kind)) if kind == "computer")
        );
    }

    async fn wire(message: &ModelMessage) -> Value {
        serde_json::to_value(WireMessage::prepare(message).await.expect("wire"))
            .expect("serialize wire message")
    }

    #[tokio::test]
    async fn every_message_role_keeps_its_exact_audit_json() {
        let call = ModelToolCall {
            id: "call-1".into(),
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: "bash".to_owned(),
                arguments: "{}".to_owned(),
            },
        };
        let native = AssistantTurn::new(
            Some("answer".to_owned()),
            vec![call],
            Some(ModelUsage {
                input_tokens: Some(7),
                ..ModelUsage::default()
            }),
        )
        .with_codex_continuation(
            vec![json!({"type":"reasoning","encrypted_content":"native-sentinel"})],
            ClientIdentity::new(),
            None,
        );
        let cases = [
            (
                ModelMessage::system("instructions"),
                r#"{"role":"system","content":"instructions"}"#,
            ),
            (
                ModelMessage::user("hello"),
                r#"{"role":"user","content":"hello"}"#,
            ),
            (
                ModelMessage::user_with_parts(vec![
                    ContentPart::Text("look".to_owned()),
                    ContentPart::Image {
                        mime: "image/png".to_owned(),
                        data: crate::asset::DiskBlob::from_bytes(b"PNG")
                            .expect("spool")
                            .into(),
                    },
                    ContentPart::File {
                        name: "a.pdf".to_owned(),
                        mime: "application/pdf".to_owned(),
                        data: crate::asset::DiskBlob::from_bytes(b"PDF")
                            .expect("spool")
                            .into(),
                    },
                ]),
                r#"{"role":"user","content":["look","[image/png, 3 bytes]","[a.pdf (application/pdf), 3 bytes]"]}"#,
            ),
            (
                assistant_message(&native),
                r#"{"role":"assistant","content":"answer","tool_calls":[{"id":"call-1","type":"function","function":{"name":"bash","arguments":"{}"}}]}"#,
            ),
            (
                assistant_message(&AssistantTurn::new(None, Vec::new(), None)),
                r#"{"role":"assistant"}"#,
            ),
            (
                assistant_message(&AssistantTurn::new(Some(String::new()), Vec::new(), None)),
                r#"{"role":"assistant","content":""}"#,
            ),
            (
                ModelMessage::tool("call-1", "result"),
                r#"{"role":"tool","content":"result","tool_call_id":"call-1"}"#,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                serde_json::to_string(&message).expect("audit JSON"),
                expected
            );
        }
        assert!(!format!("{native:?}").contains("native-sentinel"));
        assert!(
            matches!(assistant_message(&native), ModelMessage::Assistant { turn } if turn.codex_items() == native.codex_items())
        );
    }

    #[tokio::test]
    async fn malformed_responses_and_missing_choices_have_typed_protocol_causes() {
        use crate::error::ProtocolFailure;
        assert!(matches!(
            turn_from_response(
                crate::diagnostic::DiagnosticSecrets::default(),
                ChatResponse {
                    model: None,
                    error: None,
                    choices: Vec::new(),
                    usage: None
                }
            ),
            Err(InferenceError::Protocol(ProtocolFailure::NoChoices))
        ));
        assert!(matches!(
            replay_transcript(
                crate::diagnostic::DiagnosticSecrets::default(),
                "data: invalid\n\n",
                &mut ignored
            ),
            Err(InferenceError::Protocol(ProtocolFailure::Decode(_)))
        ));
        assert!(matches!(
            replay_transcript(
                crate::diagnostic::DiagnosticSecrets::default(),
                "",
                &mut ignored
            ),
            Err(InferenceError::Protocol(ProtocolFailure::MissingTerminal))
        ));
        assert!(
            matches!(replay_transcript(crate::diagnostic::DiagnosticSecrets::default(), "data: {\"error\":{\"message\":\"refused\"}}\n\n", &mut ignored), Err(InferenceError::Provider(failure)) if failure.status == Some(200) && failure.diagnostic == "refused")
        );
    }

    #[tokio::test]
    async fn a_text_only_message_still_serializes_to_a_bare_string() {
        assert_eq!(
            wire(&ModelMessage::user("how many files?")).await,
            json!({"role": "user", "content": "how many files?"})
        );
        assert_eq!(
            wire(&ModelMessage::tool("call-1", "42")).await,
            json!({"role": "tool", "content": "42", "tool_call_id": "call-1"})
        );
    }

    #[tokio::test]
    async fn attachments_become_chat_completions_content_parts() {
        let message = ModelMessage::user_with_parts(vec![
            ContentPart::Text("what does this say?".to_owned()),
            ContentPart::Image {
                mime: "image/png".to_owned(),
                data: crate::asset::DiskBlob::from_bytes(b"PNG")
                    .expect("spool")
                    .into(),
            },
            ContentPart::File {
                name: "spec.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                data: crate::asset::DiskBlob::from_bytes(b"PDF")
                    .expect("spool")
                    .into(),
            },
        ]);

        let clone = message.clone();
        for _ in 0..3 {
            assert_eq!(wire(&clone).await, wire(&message).await);
            assert_eq!(
                wire(&message).await,
                json!({
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "what does this say?"},
                        {"type": "image_url", "image_url": {"url": format!("data:{};base64,UE5H", "image/png")}},
                        {"type": "file", "file": {
                            "filename": "spec.pdf",
                            "file_data": "data:application/pdf;base64,UERG"
                        }},
                    ],
                })
            );
        }
    }

    #[tokio::test]
    async fn a_request_body_buffer_is_exactly_the_bytes_it_carries() {
        let body = json!({"data": "x".repeat(1_000_000)});

        let encoded = super::compact_json_body(&body).expect("the request body serializes");

        assert_eq!(
            encoded.capacity(),
            encoded.len(),
            "the buffer grew past the body it holds"
        );
        assert_eq!(
            encoded,
            serde_json::to_vec(&body).expect("the same document, compact")
        );
    }

    #[tokio::test]
    async fn an_attachment_serializes_as_the_data_url_it_has_always_been() {
        use base64::Engine as _;
        let bytes = b"\x89PNG\r\n\x1a\n".to_vec();

        let url = super::DataUrl::new("image/png", bytes.clone());

        assert_eq!(
            serde_json::to_value(&url).expect("the data URL serializes"),
            json!(format!(
                "data:{};base64,{}",
                "image/png",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            ))
        );
        assert!(
            !format!("{url:?}").contains("PNG"),
            "the debug rendering carries bytes"
        );
    }

    #[tokio::test]
    async fn an_attachment_never_reaches_the_audit_transcript_as_bytes() {
        let message = ModelMessage::user_with_parts(vec![
            ContentPart::Text("look".to_owned()),
            ContentPart::Image {
                mime: "image/png".to_owned(),
                data: crate::asset::DiskBlob::from_bytes(b"PNG")
                    .expect("spool")
                    .into(),
            },
        ]);

        let logged =
            serde_json::to_string(std::slice::from_ref(&message)).expect("serialize transcript");
        assert!(
            logged.contains("[image/png, 3 bytes]"),
            "the record should say what arrived: {logged}"
        );
        assert!(
            !logged.contains("UE5H"),
            "encoded bytes must never reach the log: {logged}"
        );
        let debugged = format!("{message:?}");
        assert!(debugged.contains("bytes: 3"), "{debugged}");
        assert!(!debugged.contains("UE5H"), "{debugged}");
        assert!(
            !debugged.contains("80, 78, 71"),
            "raw bytes leaked: {debugged}"
        );
    }

    fn frames(chunks: &[&str]) -> String {
        chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect()
    }

    struct Recorded {
        name: &'static str,
        stream: String,
        expected: Expected,
    }

    enum Expected {
        Completion(&'static str),
        MissingTerminal,
        ProviderFailure,
    }

    fn recorded_turns() -> Vec<Recorded> {
        vec![
            Recorded {
                name: "a plain text answer",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"content":"PR #7 "},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"content":"is merged."},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                    r#"{"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":100},"completion_tokens_details":{"reasoning_tokens":7}}}"#,
                    "[DONE]",
                ]),
                expected: Expected::Completion(
                    r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"PR #7 is merged."},"finish_reason":"stop"}],"usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":100},"completion_tokens_details":{"reasoning_tokens":7}}}"#,
                ),
            },
            Recorded {
                name: "one tool call, arguments in fragments, content null throughout",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"function":{"arguments":"{\"script\":"}}]},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"content":null,"tool_calls":[{"index":0,"function":{"arguments":"\"ls | wc -l\"}"}}]},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                    r#"{"choices":[],"usage":{"prompt_tokens":80,"completion_tokens":12,"total_tokens":92}}"#,
                    "[DONE]",
                ]),
                expected: Expected::Completion(
                    r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"script\":\"ls | wc -l\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":80,"completion_tokens":12,"total_tokens":92}}"#,
                ),
            },
            Recorded {
                name: "two parallel tool calls interleaved by index",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":""}}]}}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"inspect_agent_config","arguments":""}}]}}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"script\":\"date\"}"}}]}}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]}}]}"#,
                    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                    "[DONE]",
                ]),
                expected: Expected::Completion(
                    r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"bash","arguments":"{\"script\":\"date\"}"}},{"id":"call_2","type":"function","function":{"name":"inspect_agent_config","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
                ),
            },
            Recorded {
                name: "llama.cpp: a whole tool call in one fragment, and no [DONE]",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function","function":{"name":"bash","arguments":"{\"script\":\"uname -a\"}"}}]},"finish_reason":null}]}"#,
                    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                ]),
                expected: Expected::Completion(
                    r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_9","type":"function","function":{"name":"bash","arguments":"{\"script\":\"uname -a\"}"}}]},"finish_reason":"tool_calls"}]}"#,
                ),
            },
            Recorded {
                name: "Ollama: every call of a batch reported at index 0",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"bash","arguments":"{\"script\":\"date\"}"}}]}}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_b","type":"function","function":{"name":"inspect_agent_config","arguments":"{}"}}]}}]}"#,
                    r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
                    "[DONE]",
                ]),
                expected: Expected::Completion(
                    r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"bash","arguments":"{\"script\":\"date\"}"}},{"id":"call_b","type":"function","function":{"name":"inspect_agent_config","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
                ),
            },
            Recorded {
                name: "one chunk split across two data lines, usage null until the last",
                stream: format!(
                    "data: {}\ndata: {}\n\n{}",
                    r#"{"choices":[{"index":0,"delta":{"content":"one"}}],"#,
                    r#""usage":null}"#,
                    frames(&[
                        r#"{"choices":[{"index":0,"delta":{"content":" two"},"finish_reason":"stop"}],"usage":null}"#,
                        r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
                        "[DONE]",
                    ]),
                ),
                expected: Expected::Completion(
                    r#"{"choices":[{"index":0,"message":{"role":"assistant","content":"one two"},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
                ),
            },
            Recorded {
                name: "a stream the endpoint cut off mid-answer",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"content":"PR #7 "}}]}"#,
                    r#"{"choices":[{"index":0,"delta":{"content":"is"}}]}"#,
                ]),
                expected: Expected::MissingTerminal,
            },
            Recorded {
                name: "a failure reported as an event after the 200",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"content":"PR #7 "}}]}"#,
                    r#"{"error":{"message":"context length exceeded","type":"invalid_request_error"}}"#,
                ]),
                expected: Expected::ProviderFailure,
            },
        ]
    }

    #[tokio::test]
    async fn a_streamed_turn_equals_the_non_streaming_parse_of_the_same_completion() {
        for case in recorded_turns() {
            match case.expected {
                Expected::Completion(completion) => {
                    let streamed = replay_transcript(
                        crate::diagnostic::DiagnosticSecrets::default(),
                        &case.stream,
                        &mut ignored,
                    )
                    .unwrap_or_else(|error| panic!("{}: {error}", case.name));
                    let response = serde_json::from_str::<ChatResponse>(completion)
                        .unwrap_or_else(|error| panic!("{}: {error}", case.name));
                    let parsed = turn_from_response(
                        crate::diagnostic::DiagnosticSecrets::default(),
                        response,
                    )
                    .unwrap_or_else(|error| panic!("{}: {error}", case.name));

                    assert_eq!(streamed, parsed, "{}", case.name);
                }
                Expected::MissingTerminal => {
                    assert!(matches!(
                        replay_transcript(
                            crate::diagnostic::DiagnosticSecrets::default(),
                            &case.stream,
                            &mut ignored
                        ),
                        Err(InferenceError::Protocol(
                            crate::error::ProtocolFailure::MissingTerminal
                        ))
                    ));
                }
                Expected::ProviderFailure => {
                    assert!(
                        matches!(replay_transcript(crate::diagnostic::DiagnosticSecrets::default(), &case.stream, &mut ignored),
                        Err(InferenceError::Provider(failure)) if failure.status == Some(200)
                            && failure.diagnostic == "context length exceeded")
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn a_streamed_turn_reports_its_text_and_tool_calls_as_they_arrive() {
        let stream = frames(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Checking"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":" now."}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"bash","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"bash","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Continue(())
        };

        let turn = replay_transcript(
            crate::diagnostic::DiagnosticSecrets::default(),
            &stream,
            &mut sink,
        )
        .expect("a streamed turn");

        assert_eq!(turn.content.as_deref(), Some("Checking now."));
        assert_eq!(
            recorded(&events),
            vec!["text:Checking", "text: now.", "call:0", "call:1"],
            "an event carries a fragment of the answer or a counter, and nothing else"
        );
    }

    #[test]
    fn interleaved_fragments_of_many_calls_merge_by_index() {
        let calls = 500;
        let fragment = |index: usize, arguments: &str| {
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"function":{"arguments":arguments}}]}}]}).to_string()
        };
        let mut chunks: Vec<String> = (0..calls)
            .map(|index| json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"id":format!("call_{index}"),"type":"function","function":{"name":"bash","arguments":"{\"n\":\""}}]}}]}).to_string())
            .collect();
        for _ in 0..20 {
            chunks.extend((0..calls).map(|index| fragment(index, "x")));
        }
        chunks.extend((0..calls).map(|index| fragment(index, "\"}")));
        chunks.push(
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}).to_string(),
        );
        chunks.push("[DONE]".to_owned());
        let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();

        let turn = replay_transcript(
            crate::diagnostic::DiagnosticSecrets::default(),
            &frames(&chunks),
            &mut ignored,
        )
        .expect("a streamed turn");

        assert_eq!(turn.tool_calls.len(), calls);
        for call in &turn.tool_calls {
            assert_eq!(
                call.function.arguments,
                format!("{{\"n\":\"{}\"}}", "x".repeat(20))
            );
        }
    }

    #[tokio::test]
    async fn a_callback_that_breaks_abandons_the_streamed_turn_rather_than_shortening_it() {
        let stream = frames(&[
            r#"{"choices":[{"index":0,"delta":{"content":"half an"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":" answer"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ]);
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Break(())
        };

        let error = replay_transcript(
            crate::diagnostic::DiagnosticSecrets::default(),
            &stream,
            &mut sink,
        )
        .expect_err("the caller stopped");

        assert!(matches!(error, InferenceError::Cancelled));
        assert_eq!(
            recorded(&events),
            vec!["text:half an"],
            "reading continued past the break"
        );
    }

    fn request_body(request: &str) -> Value {
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("a recorded request with a body");
        serde_json::from_str(body).expect("a JSON request body")
    }

    #[tokio::test]
    async fn a_streaming_request_asks_for_usage_and_reads_the_answer_as_an_event_stream() {
        let server = MockServer::start(vec![MockResponse::sse(&frames(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Merged"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":" already."},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
            "[DONE]",
        ]))]);
        let model = OpenAiClient::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
            .expect("model client");
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Continue(())
        };

        let turn = generate(
            &model,
            &[ModelMessage::user("is it merged?")],
            &[],
            &CompletionOptions::default(),
            &mut sink,
        )
        .await
        .expect("a streamed turn");

        assert_eq!(turn.content.as_deref(), Some("Merged already."));
        assert_eq!(
            turn.usage.and_then(|usage| usage.total_tokens),
            Some(10),
            "without stream_options.include_usage a streamed call reports no cost at all"
        );
        assert_eq!(recorded(&events), vec!["text:Merged", "text: already."]);
        let requests = server.requests();
        let request = &requests[0];
        let body = request_body(request);
        assert_eq!(body["stream"], json!(true), "{request}");
        assert_eq!(
            body["stream_options"],
            json!({"include_usage": true}),
            "{request}"
        );
        assert!(request.contains("accept: text/event-stream"), "{request}");
    }

    #[tokio::test]
    async fn an_endpoint_with_streaming_off_gets_the_request_it_received_before_streaming_existed()
    {
        let server = MockServer::start(vec![MockResponse::json(json!({
            "choices": [{"message": {"content": "Merged already."}}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
        }))]);
        let model = OpenAiClient::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
            .expect("model client")
            .with_streaming(false);
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Continue(())
        };

        let turn = generate(
            &model,
            &[ModelMessage::user("is it merged?")],
            &[],
            &CompletionOptions::default(),
            &mut sink,
        )
        .await
        .expect("a whole turn");

        assert_eq!(turn.content.as_deref(), Some("Merged already."));
        assert!(
            recorded(&events).is_empty(),
            "an endpoint that is not streaming must report no events at all"
        );
        let requests = server.requests();
        let request = &requests[0];
        assert!(
            !request.contains(r#""stream""#),
            "a non-streaming request must not carry the field at all: {request}"
        );
        assert!(!request.contains("stream_options"), "{request}");
        assert!(request.contains("accept: application/json"), "{request}");
    }

    #[tokio::test]
    async fn a_multimodal_message_reports_parts_rather_than_partial_text() {
        let message = ModelMessage::user_with_parts(vec![ContentPart::Text("look".to_owned())]);
        assert_eq!(message.content(), None);
        assert_eq!(message.parts().map(<[_]>::len), Some(1));

        let text = ModelMessage::user("look");
        assert_eq!(text.content(), Some("look"));
        assert_eq!(text.parts(), None);
    }
    #[tokio::test]
    async fn chat_completions_released_history_is_explicit_but_io_failure_is_not_hidden() {
        struct Missing(crate::asset::BlobError);
        impl crate::asset::BlobSource for Missing {
            fn pin(&self) -> Result<crate::asset::DiskBlob, crate::asset::BlobError> {
                Err(self.0)
            }
        }
        let message_for = |error| {
            ModelMessage::user_with_parts(vec![ContentPart::Image {
                mime: "image/png".into(),
                data: crate::asset::BlobReference::new(std::sync::Arc::new(Missing(error)), 12, 7),
            }])
        };
        let message = message_for(crate::asset::BlobError::Reclaimed);
        let wire = serde_json::to_value(WireMessage::prepare(&message).await.unwrap())
            .unwrap()
            .to_string();
        assert!(
            wire.contains("gateway: Chat Asset #7 was released"),
            "{wire}"
        );
        assert!(!wire.contains("image_url"));
        let message = message_for(crate::asset::BlobError::LengthChanged);
        assert!(WireMessage::prepare(&message).await.is_err());
    }
}
