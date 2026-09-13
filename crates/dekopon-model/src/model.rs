use std::{fmt, io::Read as _, ops::ControlFlow, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_core::Redacted;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;
use ureq::{Agent, http};

use crate::{
    sse::{SseError, SseEvent, SseReader},
    stream::{ModelText, TurnEvent},
};

/// A model-facing tool definition.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelTool {
    /// OpenAI-compatible function name.
    pub name: String,
    /// Prompt-visible capability description.
    pub description: String,
    /// JSON Schema for function arguments.
    pub parameters: Value,
}

/// One piece of a multimodal message.
///
/// `Debug` and `Serialize` render bytes as a summary and never as bytes. Every message this crate
/// builds passes through the prompt transcript `dekopon-agent` writes to the audit log, and a
/// base64 screenshot in that record would be enormous, sender-supplied, and permanent. The wire
/// encoding lives in each transport's own request builder, which is the only place a data URL is
/// produced.
#[derive(Clone, PartialEq)]
pub enum ContentPart {
    /// Prose, the same thing a text-only message carries.
    Text(String),
    /// An image the model can look at.
    Image {
        /// IANA media type, such as `image/png`.
        mime: String,
        /// Raw bytes, encoded only when a request is built.
        data: Vec<u8>,
    },
    /// A document the model can read.
    File {
        /// The name the sender gave it, which is how a model tells two attachments apart.
        name: String,
        /// IANA media type, such as `application/pdf`.
        mime: String,
        /// Raw bytes, encoded only when a request is built.
        data: Vec<u8>,
    },
}

impl fmt::Debug for ContentPart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(text) => formatter.debug_tuple("Text").field(text).finish(),
            Self::Image { mime, data } => formatter
                .debug_struct("Image")
                .field("mime", mime)
                .field("bytes", &data.len())
                .finish(),
            Self::File { name, mime, data } => formatter
                .debug_struct("File")
                .field("name", name)
                .field("mime", mime)
                .field("bytes", &data.len())
                .finish(),
        }
    }
}

impl Serialize for ContentPart {
    /// The audit rendering, not the wire one. See the type's own documentation.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(text) => serializer.serialize_str(text),
            Self::Image { mime, data } => {
                serializer.serialize_str(&format!("[{mime}, {} bytes]", data.len()))
            }
            Self::File { name, mime, data } => {
                serializer.serialize_str(&format!("[{name} ({mime}), {} bytes]", data.len()))
            }
        }
    }
}

/// What a message carries: prose, or prose interleaved with attachments.
///
/// Untagged so a text-only message still renders as a bare string, which keeps every existing
/// request and every existing audit record byte-identical to what they were before parts existed.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// The text of a text-only message, or `None` when this message carries parts.
    fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Parts(_) => None,
        }
    }

    fn as_parts(&self) -> Option<&[ContentPart]> {
        match self {
            Self::Text(_) => None,
            Self::Parts(parts) => Some(parts),
        }
    }
}

/// Encodes one attachment as the `data:` URL both wire formats accept.
///
/// Built at request time and dropped with the request. Nothing retains the encoded copy, which is
/// what keeps a screenshot from being held twice for the life of a conversation.
pub(crate) fn data_url(mime: &str, data: &[u8]) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(data))
}

/// One model-request conversation message.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ModelToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip)]
    replay_items: Vec<Value>,
}

impl ModelMessage {
    /// Creates a system instruction.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain("system", content)
    }

    /// Creates a user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::plain("user", content)
    }

    /// Creates a user message carrying attachments alongside its text.
    ///
    /// Separate from [`Self::user`] rather than replacing it: a text-only message must keep
    /// serializing to a bare string on both wire formats, and most messages are text-only.
    #[must_use]
    pub fn user_with_parts(parts: Vec<ContentPart>) -> Self {
        Self {
            role: "user",
            content: Some(MessageContent::Parts(parts)),
            tool_calls: Vec::new(),
            tool_call_id: None,
            replay_items: Vec::new(),
        }
    }

    /// Creates a tool result message.
    #[must_use]
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool",
            content: Some(MessageContent::Text(content.into())),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            replay_items: Vec::new(),
        }
    }

    fn assistant(turn: &AssistantTurn) -> Self {
        Self {
            role: "assistant",
            content: turn.content.clone().map(MessageContent::Text),
            tool_calls: turn.tool_calls.clone(),
            tool_call_id: None,
            replay_items: turn.replay_items.clone(),
        }
    }

    fn plain(role: &'static str, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(MessageContent::Text(content.into())),
            tool_calls: Vec::new(),
            tool_call_id: None,
            replay_items: Vec::new(),
        }
    }

    /// Returns the wire role.
    #[must_use]
    pub const fn role(&self) -> &'static str {
        self.role
    }

    /// Returns message text, or `None` when the message is absent or carries attachments.
    ///
    /// A message with parts answers `None` rather than its text run, because a caller that wanted
    /// the whole content and silently received only part of it is the worse failure. Reach for
    /// [`Self::parts`] when attachments matter.
    #[must_use]
    pub fn content(&self) -> Option<&str> {
        self.content.as_ref().and_then(MessageContent::as_text)
    }

    /// Returns the attachments and text runs of a multimodal message, if it is one.
    #[must_use]
    pub fn parts(&self) -> Option<&[ContentPart]> {
        self.content.as_ref().and_then(MessageContent::as_parts)
    }

    pub(crate) fn tool_calls(&self) -> &[ModelToolCall] {
        &self.tool_calls
    }

    pub(crate) fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    pub(crate) fn replay_items(&self) -> &[Value] {
        &self.replay_items
    }
}

/// A tool call emitted by a chat model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelToolCall {
    /// Endpoint-assigned call ID used to correlate the tool result.
    pub id: String,
    /// OpenAI tool kind; currently required to be `function`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Function name and JSON argument text.
    pub function: ModelFunctionCall,
}

/// Function details nested inside a model tool call.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelFunctionCall {
    /// Prompt-visible function name.
    pub name: String,
    /// JSON-encoded function arguments.
    pub arguments: String,
}

/// Token accounting for one billed model call, normalized across transports.
///
/// Every field is what the provider reported, or `None` when it reported nothing: these numbers
/// determine cost, so inventing a zero would turn "the API said nothing" into "the API said free".
/// Chat-completions responses call the halves `prompt_tokens`/`completion_tokens`; the Codex
/// Responses API calls them `input_tokens`/`output_tokens`. Both normalize to the latter here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelUsage {
    /// Tokens the request consumed, cached and uncached alike.
    pub input_tokens: Option<u64>,
    /// The subset of input tokens served from the provider's prompt cache.
    pub cached_input_tokens: Option<u64>,
    /// Tokens the response produced, reasoning included.
    pub output_tokens: Option<u64>,
    /// The subset of output tokens spent on reasoning.
    pub reasoning_output_tokens: Option<u64>,
    /// Provider-reported total for the call.
    pub total_tokens: Option<u64>,
}

/// One assistant response, which may contain text or tool calls.
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantTurn {
    /// Assistant text, if any.
    pub content: Option<String>,
    /// Requested tool calls.
    pub tool_calls: Vec<ModelToolCall>,
    /// Token accounting for this call, when the provider reported it.
    pub usage: Option<ModelUsage>,
    /// Provider-specific opaque response items required for safe replay.
    #[doc(hidden)]
    pub replay_items: Vec<Value>,
}

/// Request-scoped routing metadata for one model call.
///
/// Deliberately separate from `messages` and `tools`: nothing here changes what the model is
/// asked, only how the provider routes the request that carries it. Every field is optional and a
/// transport that does not understand one omits it, so the worst outcome of a field going
/// unrecognized is that the request costs more — never that it answers differently.
///
/// Options are passed per request rather than stored on a client. The model client is currently
/// rebuilt for each gateway message, and the obvious optimization is to share one client across
/// sessions; a value captured in a constructor would then describe the first conversation forever
/// while quietly mislabeling every later one.
///
/// Fields are private so later routing metadata can join this struct without breaking callers that
/// build it with [`CompletionOptions::default`] and the `with_*` methods.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionOptions {
    prompt_cache_key: Option<String>,
}

impl CompletionOptions {
    /// Groups this request with earlier requests carrying the same key.
    ///
    /// The key is a hint for the provider's automatic prefix cache: it tells the backend which
    /// requests are likely to share a leading prefix so they can be routed to the same cache. It
    /// is **not** an access-control boundary and grants nothing — the request still carries the
    /// whole conversation, and a backend that ignores the field returns a byte-identical answer at
    /// full price. Choose a value that is stable for one conversation and unshared between
    /// unrelated ones; a key reused across conversations only wastes cache lookups.
    ///
    /// A blank key is dropped rather than sent, so a caller that computes an empty identifier
    /// leaves the request exactly as it would have been with no key at all.
    #[must_use]
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        let key = key.into();
        self.prompt_cache_key = (!key.trim().is_empty()).then_some(key);
        self
    }

    /// Returns the prompt cache key when one is set.
    #[must_use]
    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }
}

/// Synchronous model boundary used by the immediate prompt loop.
pub trait ChatModel: Send + Sync {
    /// Requests the next assistant turn, reporting what arrives while it arrives.
    ///
    /// One method, not two. Streaming is not a mode a caller opts into here: an implementation
    /// that cannot stream — a test double, an endpoint configured with `stream: false` — calls
    /// `on_event` zero times and returns the same [`AssistantTurn`] it always did. A caller
    /// therefore never has two paths to keep in agreement, which is what a second entry point
    /// costs in practice.
    ///
    /// `on_event` runs on this thread, in arrival order, between reads. Returning
    /// [`ControlFlow::Break`] stops the turn: the response body is dropped, which closes the
    /// connection rather than returning it to the pool, and the call answers
    /// [`ModelError::Interrupted`]. Whatever text had already been delivered is the caller's — the
    /// turn itself is gone, and nothing partial is returned in its place.
    ///
    /// What cannot be interrupted is a read that is waiting on a silent socket. `Break` is
    /// observed between events, so a backend in a phase that emits none — the Codex reasoning
    /// phase, tens of seconds of it — stops only when the client's global deadline fires.
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
    ) -> Result<AssistantTurn, ModelError>;
}

/// OpenAI-compatible chat-completions client.
pub struct OpenAiChatModel {
    agent: Agent,
    endpoint: String,
    model: String,
    bearer_token: Option<Redacted<String>>,
    stream: bool,
}

impl OpenAiChatModel {
    /// Creates a bounded blocking client.
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        bearer_token: Option<String>,
        timeout: Duration,
    ) -> Result<Self, ModelError> {
        if timeout.is_zero() {
            return Err(ModelError::Configuration(
                "model timeout must be greater than zero".to_owned(),
            ));
        }
        let endpoint = endpoint.into();
        if endpoint.trim().is_empty() {
            return Err(ModelError::Configuration(
                "model endpoint must not be empty".to_owned(),
            ));
        }
        let model = model.into();
        if model.trim().is_empty() {
            return Err(ModelError::Configuration(
                "model name must not be empty".to_owned(),
            ));
        }

        let agent = crate::agent(timeout);
        let bearer_token = bearer_token.and_then(|token| {
            let token = token.trim().to_owned();
            (!token.is_empty()).then_some(Redacted::new(token))
        });
        if bearer_token.is_some() && !allows_bearer_token(&endpoint) {
            return Err(ModelError::Configuration(
                "bearer tokens require HTTPS or a loopback HTTP endpoint".to_owned(),
            ));
        }

        Ok(Self {
            agent,
            endpoint: completion_url(&endpoint),
            model,
            bearer_token,
            stream: true,
        })
    }

    /// Chooses whether this endpoint is asked to stream its answer. Streaming is the default.
    ///
    /// The escape hatch for an endpoint that claims chat-completions compatibility and gets
    /// `stream: true` wrong — a proxy that buffers the whole SSE body, a server that drops
    /// `usage`. With streaming off the request omits the field entirely rather than sending
    /// `false`, so such an endpoint receives the request it received before streaming existed, and
    /// the turn's callback is never called.
    #[must_use]
    pub fn with_streaming(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }
}

impl ChatModel for OpenAiChatModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
    ) -> Result<AssistantTurn, ModelError> {
        let span = tracing::info_span!(
            "model.complete",
            model = %self.model,
            message.count = messages.len(),
            tool.count = tools.len(),
            model.stream = self.stream
        );
        let _entered = span.enter();

        let tools = tools
            .iter()
            .map(|tool| OpenAiTool {
                kind: "function",
                function: tool,
            })
            .collect::<Vec<_>>();
        let wire = messages.iter().map(WireMessage::from).collect::<Vec<_>>();
        let request_body = ChatRequest {
            model: &self.model,
            messages: &wire,
            tools: &tools,
            tool_choice: "auto",
            prompt_cache_key: options.prompt_cache_key(),
            stream: self.stream.then_some(true),
            // Without this a streamed turn reports no usage at all, and a call whose cost the
            // provider did not report is a call Dekopon cannot price.
            stream_options: self.stream.then_some(StreamOptions {
                include_usage: true,
            }),
        };

        let mut request = self.agent.post(&self.endpoint).header(
            "accept",
            if self.stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
        if let Some(token) = &self.bearer_token {
            // One of the few places a credential leaves its wrapper, and it goes straight onto the
            // wire rather than into a variable that could later be formatted somewhere else.
            request = request.header("authorization", &format!("Bearer {}", token.expose()));
        }
        let mut response = request
            .send_json(&request_body)
            .map_err(|error| ModelError::Request(error.to_string()))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let detail = read_error_body(response);
            return Err(ModelError::Request(format!("HTTP {status}: {detail}")));
        }
        if !self.stream {
            let response = response
                .body_mut()
                .read_json::<ChatResponse>()
                .map_err(|error| ModelError::Response(error.to_string()))?;
            return turn_from_response(response);
        }
        // An endpoint that ignores `stream: true` answers with one JSON document. Reading that as
        // an event stream fails only at its end with "stream ended before [DONE]", which names
        // neither the endpoint's behaviour nor the one-line fix, so the content type is checked
        // first and the refusal names the key to write.
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !content_type.starts_with("text/event-stream") {
            let shown = if content_type.is_empty() {
                "no content type".to_owned()
            } else {
                format!("`{content_type}`")
            };
            return Err(ModelError::Response(format!(
                "streaming was requested but the endpoint answered with {shown}; write \
                 `stream: false` on this model for an endpoint that ignores `stream: true`"
            )));
        }
        read_chat_stream(response.into_parts().1.into_reader(), on_event)
    }
}

/// The one place a finished chat-completions response becomes a turn.
///
/// Shared with the streaming accumulator's own conversion by the table test that asserts the two
/// agree: a streamed turn and the non-streaming parse of the same completion are the same value,
/// and the only way to keep that true is for the rules about what `content` and `tool_calls` mean
/// to have one home each.
fn turn_from_response(response: ChatResponse) -> Result<AssistantTurn, ModelError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(ModelError::NoChoices)?;
    let tool_calls = choice
        .message
        .tool_calls
        .into_iter()
        .map(ModelToolCall::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(AssistantTurn {
        content: choice.message.content,
        tool_calls,
        usage: response.usage.map(ModelUsage::from),
        replay_items: Vec::new(),
    })
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [WireMessage<'a>],
    tools: &'a [OpenAiTool<'a>],
    tool_choice: &'static str,
    /// Skipped when absent so a request without a cache key serializes to the same bytes it did
    /// before the field existed. Compatible endpoints that have never heard of it ignore it.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
    /// `Some(true)` or absent, never `Some(false)`: an endpoint that does not stream is asked the
    /// question it was asked before the field existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One message as the chat-completions wire wants it.
///
/// A separate type from [`ModelMessage`] because the two answer different questions. This is what
/// an endpoint parses; `ModelMessage`'s own `Serialize` is the redacted rendering that reaches the
/// audit transcript. While they were one type, the wire format *was* the log format, which put a
/// base64 attachment one careless `to_string` away from being written to disk forever.
///
/// Field order and skip rules match what the derived implementation emitted before this type
/// existed, so a text-only request serializes to the same bytes it always did.
#[derive(Debug, Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<WireContent<'a>>,
    #[serde(skip_serializing_if = "is_empty")]
    tool_calls: &'a [ModelToolCall],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// Serde needs a named predicate to skip an empty borrowed slice.
fn is_empty(calls: &&[ModelToolCall]) -> bool {
    calls.is_empty()
}

/// Untagged, so text stays a bare string and only an attachment forces the array form.
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
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireUrl },
    #[serde(rename = "file")]
    File { file: WireFile<'a> },
}

#[derive(Debug, Serialize)]
struct WireUrl {
    url: String,
}

#[derive(Debug, Serialize)]
struct WireFile<'a> {
    filename: &'a str,
    file_data: String,
}

impl<'a> From<&'a ModelMessage> for WireMessage<'a> {
    fn from(message: &'a ModelMessage) -> Self {
        let content = message.content.as_ref().map(|content| match content {
            MessageContent::Text(text) => WireContent::Text(text),
            MessageContent::Parts(parts) => WireContent::Parts(
                parts
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text(text) => WirePart::Text { text },
                        ContentPart::Image { mime, data } => WirePart::ImageUrl {
                            image_url: WireUrl {
                                url: data_url(mime, data),
                            },
                        },
                        ContentPart::File { name, mime, data } => WirePart::File {
                            file: WireFile {
                                filename: name,
                                file_data: data_url(mime, data),
                            },
                        },
                    })
                    .collect(),
            ),
        });
        Self {
            role: message.role,
            content,
            tool_calls: &message.tool_calls,
            tool_call_id: message.tool_call_id.as_deref(),
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenAiTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: &'a ModelTool,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<WireChatUsage>,
}

/// Chat-completions `usage` object, including the detail blocks that carry cache and reasoning
/// counts. Every field defaults: a compatible endpoint that omits any of them still bills for the
/// rest, so a partial report is worth keeping.
#[derive(Debug, Deserialize)]
struct WireChatUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<WireCompletionTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct WirePromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireCompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<WireChatUsage> for ModelUsage {
    fn from(usage: WireChatUsage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
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

#[derive(Debug, Deserialize)]
struct ChatChoice {
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
struct WireToolCall {
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

impl TryFrom<WireToolCall> for ModelToolCall {
    type Error = ModelError;

    fn try_from(call: WireToolCall) -> Result<Self, Self::Error> {
        if call.kind != "function" {
            return Err(ModelError::UnsupportedToolKind(call.kind));
        }
        let arguments = match call.function.arguments {
            Value::String(arguments) => arguments,
            arguments @ Value::Object(_) => serde_json::to_string(&arguments)
                .map_err(|error| ModelError::Response(error.to_string()))?,
            other => {
                return Err(ModelError::Response(format!(
                    "tool arguments for {} must be a JSON string or object, found {other}",
                    call.function.name
                )));
            }
        };

        Ok(Self {
            id: call.id,
            kind: call.kind,
            function: ModelFunctionCall {
                name: call.function.name,
                arguments,
            },
        })
    }
}

/// Reads a streamed chat-completions turn, reporting text and tool calls as they arrive.
///
/// The reader is a local, so every return drops it — which is what makes [`ControlFlow::Break`] a
/// cancellation rather than a request to ignore the rest: the body is dropped mid-response and the
/// connection closes instead of going back to the pool with an unread answer in it.
pub(crate) fn read_chat_stream(
    reader: impl std::io::Read,
    on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
) -> Result<AssistantTurn, ModelError> {
    let mut events = SseReader::new(reader);
    let mut state = ChatStream::default();
    while let Some(event) = events.next_event()? {
        let SseEvent::Data(data) = event else {
            state.finished = true;
            break;
        };
        let chunk = serde_json::from_str::<ChatChunk>(data)
            .map_err(|error| ModelError::Response(format!("invalid stream chunk: {error}")))?;
        if state.apply(chunk, on_event)?.is_break() {
            return Err(ModelError::Interrupted);
        }
    }
    if !state.finished {
        return Err(ModelError::Response(
            "stream ended before [DONE] or a finish reason".to_owned(),
        ));
    }
    state.into_turn()
}

/// One `data:` chunk of a chat-completions stream.
///
/// Every field is optional and every absence is tolerated, because "OpenAI-compatible" is a claim
/// rather than a specification: llama.cpp, Ollama, vLLM, and a dozen proxies each omit or null a
/// different one, and a turn that arrived intact must not fail on the shape of a field nobody
/// reads.
#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    /// Present on the final chunk when `stream_options.include_usage` was sent, `null` on every
    /// earlier chunk from endpoints that send the field unconditionally.
    #[serde(default)]
    usage: Option<WireChatUsage>,
    /// Some endpoints report a mid-stream failure as an event instead of a status, the response
    /// having already been committed with a 200.
    #[serde(default)]
    error: Option<ChunkError>,
}

#[derive(Debug, Deserialize)]
struct ChunkError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Option<ChunkDelta>,
    /// `stop`, `tool_calls`, `length`; the one signal that this choice is complete.
    #[serde(default)]
    finish_reason: Option<String>,
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
    /// Which call this fragment belongs to. Absent means the first: an endpoint that reports one
    /// call at a time has nothing to number.
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

/// A tool call being assembled from fragments.
#[derive(Debug)]
struct StreamedCall {
    index: u64,
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

/// The turn a chat-completions stream is building.
#[derive(Debug, Default)]
struct ChatStream {
    content: String,
    calls: Vec<StreamedCall>,
    usage: Option<ModelUsage>,
    finished: bool,
}

impl ChatStream {
    /// Folds one chunk in, reporting what it contained.
    fn apply(
        &mut self,
        chunk: ChatChunk,
        on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, ModelError> {
        if let Some(error) = chunk.error {
            return Err(ModelError::Request(format!(
                "stream reported an error: {}",
                sanitize_diagnostic(error.message.as_deref().unwrap_or("no message"))
            )));
        }
        // Last report wins, which is the final usage-only chunk on an endpoint that sends one and
        // the last running total on an endpoint that repeats it.
        if let Some(usage) = chunk.usage {
            self.usage = Some(ModelUsage::from(usage));
        }
        for choice in chunk.choices {
            if choice.finish_reason.is_some() {
                self.finished = true;
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

    /// Merges one tool-call fragment, answering the turn position of a call it started.
    fn merge_call(&mut self, fragment: ChunkToolCall) -> Option<u32> {
        let index = fragment.index.unwrap_or(0);
        let function = fragment.function.unwrap_or_default();
        let name = function.name.filter(|name| !name.is_empty());
        let arguments = function.arguments.unwrap_or_default();

        let slot = self.calls.iter().rposition(|call| call.index == index);
        let slot = match slot {
            // Ollama reports every call of a parallel batch at index 0. A fragment that names a
            // function when the call already at that index has one is a second call, not more of
            // the first; anything else is a continuation, which is what OpenAI sends.
            Some(position) if name.is_some() && !self.calls[position].name.is_empty() => None,
            other => other,
        };
        let Some(position) = slot else {
            self.calls.push(StreamedCall {
                index,
                id: fragment.id.unwrap_or_default(),
                kind: fragment.kind.unwrap_or_default(),
                name: name.unwrap_or_default(),
                arguments,
            });
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

    fn into_turn(self) -> Result<AssistantTurn, ModelError> {
        let tool_calls = self
            .calls
            .into_iter()
            .map(|call| {
                // An endpoint that omits `type` on its fragments means the only kind these
                // requests can produce; one that names another kind is refused exactly as the
                // non-streaming parser refuses it.
                if !call.kind.is_empty() && call.kind != "function" {
                    return Err(ModelError::UnsupportedToolKind(call.kind));
                }
                Ok(ModelToolCall {
                    id: call.id,
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: call.name,
                        arguments: call.arguments,
                    },
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(AssistantTurn {
            content: (!self.content.is_empty()).then_some(self.content),
            tool_calls,
            usage: self.usage,
            replay_items: Vec::new(),
        })
    }
}

impl From<SseError> for ModelError {
    fn from(error: SseError) -> Self {
        match &error {
            SseError::TooLarge => Self::Response(error.to_string()),
            // A socket that died mid-body is a failed request, not a malformed answer, and the
            // caller acts on that difference.
            SseError::Read { source } => Self::Request(format!("{error}: {source}")),
        }
    }
}

/// Whether a bearer token may accompany requests to this endpoint.
///
/// The connection host must be derived exactly as the transport derives it. `Uri::host` excludes
/// userinfo, so an authority such as `127.0.0.1:80@models.example.test` resolves to the remote
/// host it actually connects to rather than the loopback literal it imitates. `Uri::host` returns
/// IPv6 literals bracketed and does not normalize case, so both are handled here.
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

/// Failure while requesting or decoding a model turn.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ModelError {
    /// Client configuration was invalid.
    #[error("invalid model configuration: {0}")]
    Configuration(String),
    /// The HTTP request failed or returned an error status.
    #[error("model request failed: {0}")]
    Request(String),
    /// The endpoint response was malformed.
    #[error("invalid model response: {0}")]
    Response(String),
    /// The response contained no choices.
    #[error("model response contained no choices")]
    NoChoices,
    /// The model returned a tool kind Dekopon's prompt loop does not execute.
    #[error("model returned unsupported tool kind {0:?}")]
    UnsupportedToolKind(String),
    /// The caller's event callback asked to stop, and the response body was dropped.
    ///
    /// A cancellation, not a failure: the request was fine and the answer was on its way. The
    /// caller keeps whatever text it was handed before it said stop; this crate keeps nothing,
    /// because half a turn is not a turn and must never reach a conversation history.
    #[error("model turn interrupted by its caller")]
    Interrupted,
}

/// Converts an assistant turn into replayable conversation state.
#[must_use]
pub fn assistant_message(turn: &AssistantTurn) -> ModelMessage {
    ModelMessage::assistant(turn)
}

/// Bound on how much of a failed response is kept as a diagnostic.
///
/// Large enough for an OpenAI-shaped error object, small enough that an endpoint answering with an
/// HTML error page cannot push a megabyte into a log line.
const MAX_ERROR_BODY_BYTES: u64 = 16 * 1024;

/// Reads the body of a non-2xx response as a bounded, log-safe diagnostic.
///
/// Every transport in this crate sets `http_status_as_error(false)` precisely so this is reachable:
/// `ureq`'s own status error renders as `http status: 429` and discards the one part of the
/// response that says what went wrong.
pub(crate) fn read_error_body(response: http::Response<ureq::Body>) -> String {
    let mut body = response
        .into_parts()
        .1
        .into_reader()
        .take(MAX_ERROR_BODY_BYTES);
    let mut text = String::new();
    #[allow(
        clippy::let_underscore_must_use,
        reason = "best-effort diagnostic read on a path that has already failed; a short or \
                  interrupted body leaves whatever arrived in `text`, and reporting the read \
                  error instead of the service's own message would lose the useful half"
    )]
    let _ = body.read_to_string(&mut text);
    let text = sanitize_diagnostic(&text);
    if text.trim().is_empty() {
        return "no response body".to_owned();
    }
    text
}

/// Strips control characters so endpoint-supplied text cannot forge log structure.
pub(crate) fn sanitize_diagnostic(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::time::Duration;

    use serde_json::Value;

    use super::{
        AssistantTurn, ChatModel, ChatRequest, ChatResponse, CompletionOptions, ContentPart,
        ControlFlow, ModelError, ModelFunctionCall, ModelMessage, ModelTool, ModelToolCall,
        ModelUsage, OpenAiChatModel, OpenAiTool, StreamOptions, TurnEvent, WireFunctionCall,
        WireMessage, WireToolCall, assistant_message, completion_url, read_chat_stream,
        turn_from_response,
    };
    use crate::mock::{MockResponse, MockServer};

    /// The callback for a turn whose deltas are not what the test is about.
    fn ignored(_event: TurnEvent) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    /// Every event a turn reported, in order, rendered so a test can assert on them.
    fn recorded(events: &[TurnEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                TurnEvent::TextDelta(text) => format!("text:{}", text.as_str()),
                TurnEvent::ToolCallStarted { index } => format!("call:{index}"),
            })
            .collect()
    }

    /// `ureq`'s own status error renders as `http status: 429` and discards the body, which is the
    /// only part of a failure that says whether the model name is wrong, the context is too long,
    /// or which rate limit was hit.
    #[test]
    fn a_failed_completion_reports_the_endpoints_own_error_body() {
        let server = MockServer::start(vec![MockResponse::failure(
            429,
            json!({"error": {"message": "Rate limit reached for gpt-test", "type": "rate_limit"}}),
        )]);
        let model =
            OpenAiChatModel::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
                .expect("model client");

        let error = model
            .complete(
                &[ModelMessage::user("hello")],
                &[],
                &CompletionOptions::default(),
                &mut ignored,
            )
            .expect_err("a 429 must fail the turn");

        let message = error.to_string();
        assert!(message.contains("429"), "{message}");
        assert!(
            message.contains("Rate limit reached for gpt-test"),
            "{message}"
        );
    }

    #[test]
    fn an_endpoint_that_ignores_stream_true_is_refused_naming_the_key_to_write() {
        // A whole JSON completion where an event stream was asked for: what a stub or a buffering
        // proxy answers. The turn fails at once, and the error names `stream: false` rather than
        // the end of a stream that never was one.
        let server = MockServer::start(vec![MockResponse::json(
            json!({"choices": [{"message": {"role": "assistant", "content": "hello"}}]}),
        )]);
        let model =
            OpenAiChatModel::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
                .expect("model client");
        assert!(
            model.stream,
            "streaming is the default for a compatible endpoint"
        );

        let error = model
            .complete(
                &[ModelMessage::user("hello")],
                &[],
                &CompletionOptions::default(),
                &mut ignored,
            )
            .expect_err("a JSON answer to a streamed request fails the turn");

        let message = error.to_string();
        assert!(message.contains("application/json"), "{message}");
        assert!(message.contains("`stream: false`"), "{message}");
        assert!(
            !message.contains("[DONE]"),
            "the refusal names the cause, not the symptom: {message}"
        );
    }

    #[test]
    fn appends_chat_completions_to_api_bases() {
        assert_eq!(
            completion_url("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            completion_url("https://example.test/chat/completions"),
            "https://example.test/chat/completions"
        );
    }

    #[test]
    fn refuses_bearer_tokens_over_remote_plaintext_http() {
        let error = OpenAiChatModel::new(
            "http://models.example.test/v1",
            "test-model",
            Some("secret".to_owned()),
            Duration::from_secs(1),
        )
        .err()
        .expect("remote bearer tokens require TLS");

        assert!(matches!(error, ModelError::Configuration(_)));

        // Userinfo makes the authority read as loopback while the socket connects elsewhere.
        for disguised in [
            "http://127.0.0.1:80@models.example.test/v1",
            "http://localhost@models.example.test/v1",
            "http://[::1]@models.example.test/v1",
        ] {
            let error = OpenAiChatModel::new(
                disguised,
                "test-model",
                Some("secret".to_owned()),
                Duration::from_secs(1),
            )
            .err()
            .unwrap_or_else(|| panic!("{disguised} connects to a remote host in plaintext"));
            assert!(matches!(error, ModelError::Configuration(_)));
        }

        for loopback in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:11434/v1",
            "http://[::1]:11434/v1",
        ] {
            assert!(
                OpenAiChatModel::new(
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
            OpenAiChatModel::new(
                "https://models.example.test/v1",
                "test-model",
                Some("secret".to_owned()),
                Duration::from_secs(1),
            )
            .is_ok()
        );
    }

    #[test]
    fn normalizes_chat_completion_usage() {
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
                cached_input_tokens: Some(100),
                output_tokens: Some(30),
                reasoning_output_tokens: Some(7),
                total_tokens: Some(150),
            }
        );
    }

    #[test]
    fn keeps_bare_usage_counts_from_minimal_endpoints() {
        // llama.cpp and friends report the three counts with no detail blocks.
        let response: ChatResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
        }))
        .expect("bare usage deserializes");

        assert_eq!(
            ModelUsage::from(response.usage.expect("usage present")),
            ModelUsage {
                input_tokens: Some(8),
                cached_input_tokens: None,
                output_tokens: Some(2),
                reasoning_output_tokens: None,
                total_tokens: Some(10),
            }
        );
    }

    #[test]
    fn accepts_object_arguments_from_compatible_endpoints() {
        let call = ModelToolCall::try_from(WireToolCall {
            id: "call-1".to_owned(),
            kind: "function".to_owned(),
            function: WireFunctionCall {
                name: "echo_echo".to_owned(),
                arguments: json!({"message": "hi"}),
            },
        })
        .expect("object arguments normalize");

        assert_eq!(call.function.arguments, r#"{"message":"hi"}"#);
    }

    /// Serializes one request fragment so the comparison is over the bytes a provider's prefix
    /// cache would hash. Both sides come from this same binary, so key ordering — which
    /// `serde_json`'s `preserve_order` feature makes a per-binary property — cannot make the
    /// assertion fail for a reason unrelated to the property under test.
    fn request_text(fragment: &Value) -> String {
        serde_json::to_string(fragment).expect("serialize request fragment")
    }

    #[test]
    fn an_appended_turn_extends_the_chat_request_without_disturbing_its_prefix() {
        // The chat-completions transport serializes `messages` verbatim and in order, hoisting
        // nothing, so an append-only history is an append-only request. Nothing enforces that but
        // this test. A conversation feature has to be correct on both backends, and this is the
        // half where a regression is hardest to notice: the request stays valid, the answers stay
        // right, and the only symptom is that the provider's prompt cache stops hitting.
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
                    messages: &messages.iter().map(WireMessage::from).collect::<Vec<_>>(),
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
            messages.push(assistant_message(&AssistantTurn {
                content: None,
                tool_calls: vec![ModelToolCall {
                    id: call_id.clone(),
                    kind: "function".to_owned(),
                    function: ModelFunctionCall {
                        name: "bash".to_owned(),
                        arguments: r#"{"script":"ls | wc -l"}"#.to_owned(),
                    },
                }],
                usage: None,
                replay_items: Vec::new(),
            }));
            messages.push(ModelMessage::tool(call_id, "12\n"));
        }

        // Unlike the Codex transport, the system message keeps its authored position instead of
        // being lifted into a separate top-level field, so growth here really is only growth.
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

    #[test]
    fn a_chat_request_carries_a_cache_key_only_when_one_is_set() {
        // Same contract as the Codex transport: absent means the field is gone, not null, so an
        // OpenAI-compatible endpoint that has never heard of `prompt_cache_key` keeps receiving
        // the request it always received.
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
        let request = |prompt_cache_key| {
            serde_json::to_value(ChatRequest {
                model: "test-model",
                messages: &messages.iter().map(WireMessage::from).collect::<Vec<_>>(),
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

    #[test]
    fn a_blank_cache_key_is_dropped_rather_than_routed() {
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

    #[test]
    fn an_implementation_that_cannot_stream_answers_without_reporting_an_event() {
        // Streaming is not optional at the trait, so this is the contract every non-streaming
        // implementation honors — the test doubles elsewhere in the workspace, and this crate's
        // own client with `stream: false`. It calls `on_event` zero times and returns the whole
        // turn, so a caller never needs a second code path for "this one does not stream".
        struct WholeTurnModel;

        impl ChatModel for WholeTurnModel {
            fn complete(
                &self,
                messages: &[ModelMessage],
                _tools: &[ModelTool],
                _options: &CompletionOptions,
                _on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
            ) -> Result<AssistantTurn, ModelError> {
                Ok(AssistantTurn {
                    content: messages.last().and_then(|message| {
                        message.content().map(|content| content.to_uppercase())
                    }),
                    tool_calls: Vec::new(),
                    usage: None,
                    replay_items: Vec::new(),
                })
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

    #[test]
    fn rejects_non_function_tool_calls() {
        let error = ModelToolCall::try_from(WireToolCall {
            id: "call-1".to_owned(),
            kind: "computer".to_owned(),
            function: WireFunctionCall {
                name: "click".to_owned(),
                arguments: json!({}),
            },
        })
        .expect_err("only function tools are supported");

        assert_eq!(
            error,
            ModelError::UnsupportedToolKind("computer".to_owned())
        );
    }

    /// One message through the chat-completions wire mapping.
    fn wire(message: &ModelMessage) -> Value {
        serde_json::to_value(WireMessage::from(message)).expect("serialize wire message")
    }

    #[test]
    fn a_text_only_message_still_serializes_to_a_bare_string() {
        // The compatibility promise of the whole change. `content` became an enum, and an untagged
        // enum that guessed wrong here would silently reshape every request the daemon has ever
        // sent to an endpoint that has never heard of content parts.
        assert_eq!(
            wire(&ModelMessage::user("how many files?")),
            json!({"role": "user", "content": "how many files?"})
        );
        assert_eq!(
            wire(&ModelMessage::tool("call-1", "42")),
            json!({"role": "tool", "content": "42", "tool_call_id": "call-1"})
        );
    }

    #[test]
    fn attachments_become_chat_completions_content_parts() {
        let message = ModelMessage::user_with_parts(vec![
            ContentPart::Text("what does this say?".to_owned()),
            ContentPart::Image {
                mime: "image/png".to_owned(),
                data: b"PNG".to_vec(),
            },
            ContentPart::File {
                name: "spec.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                data: b"PDF".to_vec(),
            },
        ]);

        assert_eq!(
            wire(&message),
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "what does this say?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,UE5H"}},
                    {"type": "file", "file": {
                        "filename": "spec.pdf",
                        "file_data": "data:application/pdf;base64,UERG"
                    }},
                ],
            })
        );
    }

    #[test]
    fn an_attachment_never_reaches_the_audit_transcript_as_bytes() {
        // `dekopon-agent` logs every prompt by serializing the message slice, so `ModelMessage`'s
        // own `Serialize` is the audit rendering rather than the wire one. A base64 screenshot in
        // that record would be enormous, sender-supplied, and permanent. The wire mapping above is
        // the only thing that ever encodes.
        let message = ModelMessage::user_with_parts(vec![
            ContentPart::Text("look".to_owned()),
            ContentPart::Image {
                mime: "image/png".to_owned(),
                data: b"PNG".to_vec(),
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
        // `Debug` is the other way a message reaches a log, and it has the same duty.
        let debugged = format!("{message:?}");
        assert!(debugged.contains("bytes: 3"), "{debugged}");
        assert!(!debugged.contains("UE5H"), "{debugged}");
        assert!(
            !debugged.contains("80, 78, 71"),
            "raw bytes leaked: {debugged}"
        );
    }

    /// Frames chunk bodies the way an endpoint puts them on the wire.
    fn frames(chunks: &[&str]) -> String {
        chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect()
    }

    /// One recorded turn from a chat-completions endpoint.
    struct Recorded {
        name: &'static str,
        stream: String,
        expected: Expected,
    }

    enum Expected {
        /// The body the same endpoint answers with when it is not asked to stream. The streamed
        /// turn must equal the parse of this, value for value.
        Completion(&'static str),
        /// The stream failed, and the surfaced cause must contain this.
        Failure(&'static str),
    }

    /// Transcripts recorded from the endpoints this client actually meets.
    ///
    /// "OpenAI-compatible" is a claim, not a specification, so the awkward ones are here on
    /// purpose: llama.cpp answering with a whole tool call in one fragment, Ollama numbering every
    /// call of a batch `0`, a proxy splitting one chunk across two `data:` lines, an endpoint
    /// nulling `usage` on every chunk but the last.
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
                expected: Expected::Failure("stream ended before [DONE] or a finish reason"),
            },
            Recorded {
                name: "a failure reported as an event after the 200",
                stream: frames(&[
                    r#"{"choices":[{"index":0,"delta":{"content":"PR #7 "}}]}"#,
                    r#"{"error":{"message":"context length exceeded","type":"invalid_request_error"}}"#,
                ]),
                expected: Expected::Failure("context length exceeded"),
            },
        ]
    }

    #[test]
    fn a_streamed_turn_equals_the_non_streaming_parse_of_the_same_completion() {
        // The property the whole feature rests on: whether or not a caller watched the deltas, the
        // turn handed back is the same value, so tool-call execution, history, and accounting
        // never learn that streaming exists. The two cut-off transcripts are here for the other
        // half of it — a turn that did not finish is a failure that names its cause, never a
        // shorter answer that looks complete.
        for case in recorded_turns() {
            match case.expected {
                Expected::Completion(completion) => {
                    let streamed = read_chat_stream(case.stream.as_bytes(), &mut ignored)
                        .unwrap_or_else(|error| panic!("{}: {error}", case.name));
                    let response = serde_json::from_str::<ChatResponse>(completion)
                        .unwrap_or_else(|error| panic!("{}: {error}", case.name));
                    let parsed = turn_from_response(response)
                        .unwrap_or_else(|error| panic!("{}: {error}", case.name));

                    assert_eq!(streamed, parsed, "{}", case.name);
                }
                Expected::Failure(cause) => {
                    let error = read_chat_stream(case.stream.as_bytes(), &mut ignored)
                        .expect_err(case.name);

                    assert!(
                        error.to_string().contains(cause),
                        "{}: {error} does not name {cause}",
                        case.name
                    );
                }
            }
        }
    }

    #[test]
    fn a_streamed_turn_reports_its_text_and_tool_calls_as_they_arrive() {
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

        let turn = read_chat_stream(stream.as_bytes(), &mut sink).expect("a streamed turn");

        assert_eq!(turn.content.as_deref(), Some("Checking now."));
        assert_eq!(
            recorded(&events),
            vec!["text:Checking", "text: now.", "call:0", "call:1"],
            "an event carries a fragment of the answer or a counter, and nothing else"
        );
    }

    #[test]
    fn a_callback_that_breaks_abandons_the_streamed_turn_rather_than_shortening_it() {
        // Failure path: the caller stopped the turn between events. Half a turn is not a turn, so
        // the answer is an interruption the caller can act on and not a truncated `AssistantTurn`
        // that would reach a conversation history looking complete.
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

        let error = read_chat_stream(stream.as_bytes(), &mut sink).expect_err("the caller stopped");

        assert_eq!(error, ModelError::Interrupted);
        assert_eq!(
            error.to_string(),
            "model turn interrupted by its caller",
            "a stopped turn must not read as an endpoint failure"
        );
        assert_eq!(
            recorded(&events),
            vec!["text:half an"],
            "reading continued past the break"
        );
    }

    /// The JSON body of a recorded request.
    ///
    /// Parsed rather than string-matched: `ureq` serializes a body with
    /// `serde_json::to_vec_pretty`, so `"stream": true` reaches the wire carrying whitespace a
    /// substring assertion would miss. The claim is about the field, not about its spelling.
    fn request_body(request: &str) -> Value {
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("a recorded request with a body");
        serde_json::from_str(body).expect("a JSON request body")
    }

    #[test]
    fn a_streaming_request_asks_for_usage_and_reads_the_answer_as_an_event_stream() {
        let server = MockServer::start(vec![MockResponse::sse(&frames(&[
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"Merged"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":" already."},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":2,"total_tokens":10}}"#,
            "[DONE]",
        ]))]);
        let model =
            OpenAiChatModel::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
                .expect("model client");
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Continue(())
        };

        let turn = model
            .complete(
                &[ModelMessage::user("is it merged?")],
                &[],
                &CompletionOptions::default(),
                &mut sink,
            )
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

    #[test]
    fn an_endpoint_with_streaming_off_gets_the_request_it_received_before_streaming_existed() {
        // The escape hatch has to be a true no-op on the wire: `stream: false` is not sent, the
        // field is absent, and a proxy that has never heard of it sees the request it always saw.
        let server = MockServer::start(vec![MockResponse::json(json!({
            "choices": [{"message": {"content": "Merged already."}}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}
        }))]);
        let model =
            OpenAiChatModel::new(server.base_url(), "gpt-test", None, Duration::from_secs(2))
                .expect("model client")
                .with_streaming(false);
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Continue(())
        };

        let turn = model
            .complete(
                &[ModelMessage::user("is it merged?")],
                &[],
                &CompletionOptions::default(),
                &mut sink,
            )
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

    #[test]
    fn a_multimodal_message_reports_parts_rather_than_partial_text() {
        // `content()` answering `Some("look")` would hand a caller the text and drop the image
        // without saying so, which is the failure this accessor split exists to prevent.
        let message = ModelMessage::user_with_parts(vec![ContentPart::Text("look".to_owned())]);
        assert_eq!(message.content(), None);
        assert_eq!(message.parts().map(<[_]>::len), Some(1));

        let text = ModelMessage::user("look");
        assert_eq!(text.content(), Some("look"));
        assert_eq!(text.parts(), None);
    }
}
