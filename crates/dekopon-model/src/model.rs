use std::{fmt, ops::ControlFlow};

use base64::{display::Base64Display, engine::general_purpose::STANDARD};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::{error::InferenceError, stream::TurnEvent};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Debug and Serialize summarize bytes rather than rendering them, since every message here passes
/// through the audit log and an embedded base64 screenshot there would be enormous,
/// sender-supplied, and permanent.
#[derive(Clone, PartialEq)]
pub enum ContentPart {
    Text(String),
    Image {
        mime: String,
        data: crate::asset::BlobReference,
    },
    File {
        name: String,
        mime: String,
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

/// MessageContent must stay serde untagged; tagging it would change the wire and audit-log shape of
/// every existing record.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl MessageContent {
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

/// Counts bytes first to size the buffer exactly, since ureq's default doubling-Vec serialization
/// allocated 89.5 MB for a 44.7 MB request body, the largest single allocation in the image path.
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

pub(crate) const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum ModelMessage {
    System {
        content: String,
    },
    User {
        content: MessageContent,
    },
    Assistant {
        #[serde(flatten)]
        turn: AssistantTurn,
    },
    #[serde(rename = "tool")]
    ToolResults {
        content: String,
        tool_call_id: ToolCallId,
    },
}

impl ModelMessage {
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: MessageContent::Text(content.into()),
        }
    }

    #[must_use]
    pub fn user_with_parts(parts: Vec<ContentPart>) -> Self {
        Self::User {
            content: MessageContent::Parts(parts),
        }
    }

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

    #[must_use]
    pub const fn role(&self) -> &'static str {
        match self {
            Self::System { .. } => "system",
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResults { .. } => "tool",
        }
    }

    #[must_use]
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::System { content } | Self::ToolResults { content, .. } => Some(content),
            Self::User { content } => content.as_text(),
            Self::Assistant { turn } => turn.content.as_deref(),
        }
    }

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ToolCallId(String);

impl ToolCallId {
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelToolCall {
    pub id: ToolCallId,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ModelFunctionCall,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelFunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Every field is None when the provider reported nothing rather than zero, since defaulting to
/// zero would misreport an unknown cost as a free one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Clone, PartialEq, Serialize)]
pub struct AssistantTurn {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ModelToolCall>,
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

/// Passed per request rather than stored on the client, since gateway sessions share a pooled
/// client and a value captured in a constructor would silently mislabel every later conversation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CompletionOptions {
    prompt_cache_key: Option<String>,
}

impl CompletionOptions {
    /// This is a cache-routing hint only, not an access-control boundary; the full conversation is
    /// still sent and an endpoint that ignores it returns the same answer at full price.
    #[must_use]
    pub fn with_prompt_cache_key(mut self, key: impl Into<String>) -> Self {
        let key = key.into();
        self.prompt_cache_key = (!key.trim().is_empty()).then_some(key);
        self
    }

    #[must_use]
    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }
}

pub trait ChatModel: Send + Sync {
    /// Returning ControlFlow::Break from on_event cancels only the local exchange; it never
    /// guarantees the pooled connection itself closes.
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        on_event: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<AssistantTurn, InferenceError>;
}

#[must_use]
pub fn assistant_message(turn: &AssistantTurn) -> ModelMessage {
    ModelMessage::assistant(turn)
}

/// Sized large enough for a typical OpenAI-shaped error object but small enough that an endpoint
/// answering with an HTML error page can't push a megabyte into a log line.
pub(crate) const MAX_ERROR_BODY_BYTES: u64 = 16 * 1024;

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
