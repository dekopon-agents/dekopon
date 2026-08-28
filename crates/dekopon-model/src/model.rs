use std::{fmt, io::Read as _, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_core::Redacted;

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use thiserror::Error;
use ureq::{Agent, http};

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
pub trait ChatModel {
    /// Requests the next assistant turn.
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError>;

    /// Requests the next assistant turn with request-scoped routing metadata.
    ///
    /// Provided rather than required so that adding routing metadata does not force every
    /// implementation — most of which are test doubles — to grow a parameter it has no use for.
    /// The default discards `options` and calls [`ChatModel::complete`], which is the safe
    /// degradation: an implementation that never learned about a field behaves exactly as it did
    /// before, because nothing in [`CompletionOptions`] is required for a correct answer.
    ///
    /// Transports that do act on options should override this method and define `complete` as
    /// delegating to it with [`CompletionOptions::default`], so one request-building path serves
    /// both entry points and the two cannot drift apart.
    fn complete_with(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
    ) -> Result<AssistantTurn, ModelError> {
        let _ = options;
        self.complete(messages, tools)
    }
}

/// OpenAI-compatible chat-completions client.
pub struct OpenAiChatModel {
    agent: Agent,
    endpoint: String,
    model: String,
    bearer_token: Option<Redacted<String>>,
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
        })
    }
}

impl ChatModel for OpenAiChatModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
        self.complete_with(messages, tools, &CompletionOptions::default())
    }

    fn complete_with(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
    ) -> Result<AssistantTurn, ModelError> {
        let span = tracing::info_span!(
            "model.complete",
            model = %self.model,
            message.count = messages.len(),
            tool.count = tools.len()
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
        };

        let mut request = self
            .agent
            .post(&self.endpoint)
            .header("accept", "application/json");
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
        let response = response
            .body_mut()
            .read_json::<ChatResponse>()
            .map_err(|error| ModelError::Response(error.to_string()))?;
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
        ModelError, ModelFunctionCall, ModelMessage, ModelTool, ModelToolCall, ModelUsage,
        OpenAiChatModel, OpenAiTool, WireFunctionCall, WireMessage, WireToolCall,
        assistant_message, completion_url,
    };
    use crate::mock::{MockResponse, MockServer};

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
            .complete(&[ModelMessage::user("hello")], &[])
            .expect_err("a 429 must fail the turn");

        let message = error.to_string();
        assert!(message.contains("429"), "{message}");
        assert!(
            message.contains("Rate limit reached for gpt-test"),
            "{message}"
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
    fn a_model_that_only_implements_complete_still_answers_through_complete_with() {
        // The whole reason `complete_with` is a provided method: a third-party or test model that
        // never heard of routing metadata keeps compiling, and ignoring the options costs it a
        // cache hit rather than an answer. This double is what the six test doubles elsewhere in
        // the workspace look like, and none of them had to change.
        struct KeylessModel;

        impl ChatModel for KeylessModel {
            fn complete(
                &self,
                messages: &[ModelMessage],
                _tools: &[ModelTool],
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

        let turn = KeylessModel
            .complete_with(
                &[ModelMessage::user("hello")],
                &[],
                &CompletionOptions::default().with_prompt_cache_key("session-7"),
            )
            .expect("a keyless implementation still answers");

        assert_eq!(turn.content.as_deref(), Some("HELLO"));
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
