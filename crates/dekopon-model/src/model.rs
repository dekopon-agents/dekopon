use std::{fmt, ops::ControlFlow};

use base64::{display::Base64Display, engine::general_purpose::STANDARD};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::{error::InferenceError, stream::TurnEvent};

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
        /// Byte-free reference, resolved only when a request is built.
        data: crate::asset::BlobReference,
    },
    /// A document the model can read.
    File {
        /// The name the sender gave it, which is how a model tells two attachments apart.
        name: String,
        /// IANA media type, such as `application/pdf`.
        mime: String,
        /// Byte-free reference, resolved only when a request is built.
        data: crate::asset::BlobReference,
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
pub enum MessageContent {
    /// A text-only user message.
    Text(String),
    /// Ordered text and attachment parts.
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

/// One attachment as the `data:` URL both wire formats accept.
///
/// Built at request time and dropped with the request. Nothing retains the encoded copy, which is
/// what keeps a screenshot from being held twice for the life of a conversation — and serializing
/// goes through `collect_str`, so the base64 form is written into the request buffer as it is
/// produced rather than existing first as a `String` a third again the size of the image.
pub(crate) struct DataUrl<'a> {
    mime: &'a str,
    data: Vec<u8>,
}

impl<'a> DataUrl<'a> {
    pub(crate) fn new(mime: &'a str, data: Vec<u8>) -> Self {
        Self { mime, data }
    }
}

impl fmt::Display for DataUrl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "data:{};base64,{}",
            self.mime,
            Base64Display::new(&self.data, &STANDARD)
        )
    }
}

/// Renders bytes as a summary, for the same reason [`ContentPart`] does.
impl fmt::Debug for DataUrl<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DataUrl")
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Serialize for DataUrl<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Counts the bytes a serialization writes without keeping any of them.
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serializes `body` as compact JSON into one buffer sized by a counting pass.
///
/// `ureq`'s `send_json` renders with `serde_json::to_vec_pretty`, and that `Vec` grows by doubling:
/// a 44.7 MB request body — one conversation carrying a few screenshots — allocated 89.5 MB, the
/// largest single allocation in the whole image path. Counting first costs a second serialization
/// pass and no allocation at all; the buffer that follows is exact, so the body exists once and its
/// `Content-Length` is known before the request opens.
pub(crate) fn compact_json_body<T>(body: &T) -> Result<Vec<u8>, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, body)?;
    let mut buffer = Vec::with_capacity(counter.0);
    serde_json::to_writer(&mut buffer, body)?;
    Ok(buffer)
}

/// The content type `ureq`'s own JSON body sets, kept because these bodies now set their own.
pub(crate) const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

/// One model-request conversation message.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ModelMessage {
    /// Instructions supplied by the caller.
    System {
        /// Instruction text.
        content: String,
    },
    /// User text and optional attachments.
    User {
        /// Ordered message content.
        content: MessageContent,
    },
    /// One completed turn, including its private native continuation.
    Assistant {
        /// The authoritative record for both audit and wire projections.
        #[serde(flatten)]
        turn: AssistantTurn,
    },
    /// A single correlated tool result, not a batch.
    #[serde(rename = "tool")]
    ToolResults {
        /// Tool output text.
        content: String,
        /// The call this output answers.
        tool_call_id: ToolCallId,
    },
}

impl ModelMessage {
    /// Creates a system instruction.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    /// Creates a user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: MessageContent::Text(content.into()),
        }
    }

    /// Creates a user message carrying attachments alongside its text.
    ///
    /// Separate from [`Self::user`] rather than replacing it: a text-only message must keep
    /// serializing to a bare string on both wire formats, and most messages are text-only.
    #[must_use]
    pub fn user_with_parts(parts: Vec<ContentPart>) -> Self {
        Self::User {
            content: MessageContent::Parts(parts),
        }
    }

    /// Creates a tool result message.
    #[must_use]
    pub fn tool(call_id: impl Into<ToolCallId>, content: impl Into<String>) -> Self {
        Self::ToolResults {
            content: content.into(),
            tool_call_id: call_id.into(),
        }
    }

    fn assistant(turn: &AssistantTurn) -> Self {
        Self::Assistant { turn: turn.clone() }
    }

    /// Returns the wire role.
    #[must_use]
    pub const fn role(&self) -> &'static str {
        match self {
            Self::System { .. } => "system",
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResults { .. } => "tool",
        }
    }

    /// Returns message text, or `None` when the message is absent or carries attachments.
    ///
    /// A message with parts answers `None` rather than its text run, because a caller that wanted
    /// the whole content and silently received only part of it is the worse failure. Reach for
    /// [`Self::parts`] when attachments matter.
    #[must_use]
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::System { content } | Self::ToolResults { content, .. } => Some(content),
            Self::User { content } => content.as_text(),
            Self::Assistant { turn } => turn.content.as_deref(),
        }
    }

    /// Returns the attachments and text runs of a multimodal message, if it is one.
    #[must_use]
    pub fn parts(&self) -> Option<&[ContentPart]> {
        match self {
            Self::User { content } => content.as_parts(),
            _ => None,
        }
    }

    pub(crate) fn tool_calls(&self) -> &[ModelToolCall] {
        match self {
            Self::Assistant { turn } => &turn.tool_calls,
            _ => &[],
        }
    }

    pub(crate) fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolResults { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }
}

/// Endpoint-assigned identity correlating a completed call and its result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Borrows the endpoint's identifier without interpreting it as authority.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ToolCallId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ToolCallId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// A tool call emitted by a chat model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelToolCall {
    /// Endpoint-assigned call ID used to correlate the tool result.
    pub id: ToolCallId,
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
    /// Input tokens written to the provider cache, when reported by OpenRouter.
    pub cache_write_tokens: Option<u64>,
    /// Tokens the response produced, reasoning included.
    pub output_tokens: Option<u64>,
    /// The subset of output tokens spent on reasoning.
    pub reasoning_output_tokens: Option<u64>,
    /// Provider-reported total for the call.
    pub total_tokens: Option<u64>,
}

/// One assistant response, which may contain text or tool calls.
#[derive(Clone, PartialEq, Serialize)]
pub struct AssistantTurn {
    /// Assistant text, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Requested tool calls.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
    /// Token accounting for this call, when the provider reported it.
    #[serde(skip)]
    pub usage: Option<ModelUsage>,
    #[serde(skip)]
    continuation: Continuation,
}

#[derive(Clone, PartialEq)]
enum Continuation {
    Portable,
    OpenRouter {
        client: ClientIdentity,
        state: crate::openrouter::Replay,
    },
    Codex {
        client: ClientIdentity,
        items: Vec<Value>,
        reported_model: Option<String>,
    },
}

#[derive(Clone)]
pub(crate) struct ClientIdentity(std::sync::Arc<()>);
impl ClientIdentity {
    pub(crate) fn new() -> Self {
        Self(std::sync::Arc::new(()))
    }
}
impl PartialEq for ClientIdentity {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl AssistantTurn {
    /// Builds a portable completed turn with no provider-native replay state.
    #[must_use]
    pub fn new(
        content: Option<String>,
        tool_calls: Vec<ModelToolCall>,
        usage: Option<ModelUsage>,
    ) -> Self {
        Self {
            content,
            tool_calls,
            usage,
            continuation: Continuation::Portable,
        }
    }

    pub(crate) fn with_codex_continuation(
        mut self,
        items: Vec<Value>,
        client: ClientIdentity,
        reported_model: Option<String>,
    ) -> Self {
        self.continuation = Continuation::Codex {
            client,
            items,
            reported_model,
        };
        self
    }

    pub(crate) fn accepts_client(&self, identity: &ClientIdentity) -> bool {
        match &self.continuation {
            Continuation::Portable => true,
            Continuation::Codex { client, .. } | Continuation::OpenRouter { client, .. } => {
                client == identity
            }
        }
    }

    pub(crate) fn codex_reported_model(&self) -> Option<&str> {
        match &self.continuation {
            Continuation::Portable | Continuation::OpenRouter { .. } => None,
            Continuation::Codex { reported_model, .. } => reported_model.as_deref(),
        }
    }

    pub(crate) fn with_openrouter_continuation(
        mut self,
        client: ClientIdentity,
        state: crate::openrouter::Replay,
    ) -> Self {
        self.continuation = Continuation::OpenRouter { client, state };
        self
    }

    pub(crate) fn openrouter_replay(&self) -> Option<&crate::openrouter::Replay> {
        match &self.continuation {
            Continuation::OpenRouter { state, .. } => Some(state),
            Continuation::Portable | Continuation::Codex { .. } => None,
        }
    }

    pub(crate) fn is_portable(&self) -> bool {
        matches!(self.continuation, Continuation::Portable)
    }

    pub(crate) fn codex_items(&self) -> Option<&[Value]> {
        match &self.continuation {
            Continuation::Portable | Continuation::OpenRouter { .. } => None,
            Continuation::Codex { items, .. } => Some(items),
        }
    }
}

impl fmt::Debug for AssistantTurn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Native reasoning belongs on the wire, never in a debug/audit projection.
        formatter
            .debug_struct("AssistantTurn")
            .field("content", &self.content)
            .field("tool_calls", &self.tool_calls)
            .field("usage", &self.usage)
            .finish_non_exhaustive()
    }
}

/// Request-scoped routing metadata for one model call.
///
/// Deliberately separate from `messages` and `tools`: nothing here changes what the model is
/// asked, only how the provider routes the request that carries it. Every field is optional and a
/// transport that does not understand one omits it, so the worst outcome of a field going
/// unrecognized is that the request costs more — never that it answers differently.
///
/// Options are passed per request rather than stored on a client. Gateway sessions share a pooled
/// client; a value captured in a constructor would describe the first conversation forever
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
    /// [`ControlFlow::Break`] cancels the local exchange and drops its response, without promising
    /// closure of the whole pooled connection. The call answers [`InferenceError::Cancelled`].
    /// Whatever text had already been delivered is the caller's — the
    /// turn itself is gone, and nothing partial is returned in its place.
    ///
    /// Production bridges also watch the session cancellation signal and total deadline, so
    /// cancellation interrupts a silent socket without waiting for another callback.
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError>;
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
pub(crate) const MAX_ERROR_BODY_BYTES: u64 = 16 * 1024;

/// Strips control characters so endpoint-supplied text cannot forge log structure.
pub(crate) fn sanitize_diagnostic(value: &str) -> String {
    truncate_diagnostic(strip_controls(value))
}

pub(crate) fn strip_controls(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

pub(crate) fn truncate_diagnostic(mut value: String) -> String {
    value.truncate(value.floor_char_boundary(MAX_ERROR_BODY_BYTES as usize));
    value
}
