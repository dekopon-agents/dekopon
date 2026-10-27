use std::{borrow::Cow, fmt, ops::ControlFlow};

use base64::{display::Base64Display, engine::general_purpose::STANDARD};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use dekopon_model_token_governor::ModelUsage;

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
            Self::Image { mime, data } => match data.sha256() {
                Some(digest) => serializer
                    .serialize_str(&format!("[{mime}, {} bytes] sha256:{digest}", data.len())),
                None => serializer.serialize_str(&format!("[{mime}, {} bytes]", data.len())),
            },
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
    #[serde(untagged)]
    Journal {
        message: crate::replay::ReplayMessage,
    },
}

impl ModelMessage {
    pub(crate) fn admits_opaque(&self, identity: &ModelKey) -> bool {
        match self {
            Self::Assistant { turn } => {
                turn.model_key() == Some(identity) && turn.has_opaque_reasoning()
            }
            Self::Journal { message } => message.admits_opaque(identity),
            Self::System { .. } | Self::User { .. } | Self::ToolResults { .. } => false,
        }
    }

    pub fn for_metering(&self) -> Cow<'_, Self> {
        match self {
            Self::User {
                content: MessageContent::Parts(parts),
            } => {
                let mut metered = None;
                for (index, part) in parts.iter().enumerate() {
                    match part {
                        ContentPart::Image { mime, data } => {
                            metered.get_or_insert_with(|| parts.clone())[index] =
                                ContentPart::Text(format!("[{mime}, {} bytes]", data.len()));
                        }
                        ContentPart::Text(_) | ContentPart::File { .. } => {}
                    }
                }
                match metered {
                    Some(parts) => Cow::Owned(Self::user_with_parts(parts)),
                    None => Cow::Borrowed(self),
                }
            }
            Self::System { .. }
            | Self::User {
                content: MessageContent::Text(_),
            }
            | Self::Assistant { .. }
            | Self::ToolResults { .. }
            | Self::Journal { .. } => Cow::Borrowed(self),
        }
    }

    pub fn prepared_for_trace(&self) -> Cow<'_, Self> {
        match self {
            Self::User {
                content: MessageContent::Parts(parts),
            } => {
                let mut prepared = None;
                for (index, part) in parts.iter().enumerate() {
                    let notice = match part {
                        ContentPart::Image { data, .. } | ContentPart::File { data, .. } => {
                            data.prepared_notice()
                        }
                        ContentPart::Text(_) => None,
                    };
                    if let Some(notice) = notice {
                        prepared.get_or_insert_with(|| parts.clone())[index] =
                            ContentPart::Text(notice);
                    }
                }
                match prepared {
                    Some(parts) => Cow::Owned(Self::user_with_parts(parts)),
                    None => Cow::Borrowed(self),
                }
            }
            Self::System { .. }
            | Self::User {
                content: MessageContent::Text(_),
            }
            | Self::Assistant { .. }
            | Self::ToolResults { .. }
            | Self::Journal { .. } => Cow::Borrowed(self),
        }
    }

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
            Self::Journal { message } => message.role(),
            Self::System { .. } => "system",
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolResults { .. } => "tool",
        }
    }

    #[must_use]
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::Journal { message } => message.content(),
            Self::System { content } | Self::ToolResults { content, .. } => Some(content),
            Self::User { content } => content.as_text(),
            Self::Assistant { turn } => turn.content.as_deref(),
        }
    }

    #[must_use]
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    pub fn parts(&self) -> Option<&[ContentPart]> {
        match self {
            Self::User { content } => content.as_parts(),
            _ => None,
        }
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
    pub(crate) fn tool_calls(&self) -> &[ModelToolCall] {
        match self {
            Self::Assistant { turn } => &turn.tool_calls,
            _ => &[],
        }
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "reshaped by the unit that next rewrites this"
    )]
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
    Portable {
        model: Option<Box<ModelKey>>,
    },
    OpenRouter {
        client: Box<ModelKey>,
        state: crate::openrouter::Replay,
    },
    Codex {
        client: Box<ModelKey>,
        items: Vec<Value>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Codex,
    OpenRouter,
    OpenAiCompatible,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UpstreamModel(String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReportedModel(String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelKey {
    backend: Backend,
    upstream: UpstreamModel,
    reported: Option<ReportedModel>,
}

impl ModelKey {
    pub fn new(backend: Backend, upstream: &str) -> Self {
        Self {
            backend,
            upstream: UpstreamModel(upstream.to_owned()),
            reported: None,
        }
    }

    pub fn with_reported(mut self, reported: Option<String>) -> Self {
        self.reported = reported.map(ReportedModel);
        self
    }

    pub(crate) fn reported(&self) -> Option<&str> {
        self.reported.as_ref().map(|model| model.0.as_str())
    }

    pub fn is_for(&self, backend: Backend, upstream: &str) -> bool {
        self.backend == backend && self.upstream.0 == upstream
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self::new(Backend::Codex, "gpt-test")
    }
}

#[derive(Clone, Serialize)]
pub struct OpaqueReasoning(Box<serde_json::value::RawValue>);

impl<'de> Deserialize<'de> for OpaqueReasoning {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(&value).map_err(serde::de::Error::custom)
    }
}

impl Eq for OpaqueReasoning {}

impl PartialEq for OpaqueReasoning {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

impl fmt::Debug for OpaqueReasoning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpaqueReasoning")
            .field("bytes", &self.bytes().len())
            .finish_non_exhaustive()
    }
}

impl OpaqueReasoning {
    pub fn from_value(value: &Value) -> Result<Self, serde_json::Error> {
        serde_json::value::to_raw_value(value).map(Self)
    }

    pub fn bytes(&self) -> &[u8] {
        self.0.get().as_bytes()
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
            continuation: Continuation::Portable { model: None },
        }
    }

    pub(crate) fn with_model_key(mut self, model: ModelKey) -> Self {
        self.continuation = Continuation::Portable {
            model: Some(Box::new(model)),
        };
        self
    }

    pub(crate) fn with_codex_continuation(
        mut self,
        items: Vec<Value>,
        client: ModelKey,
        reported_model: Option<String>,
    ) -> Self {
        self.continuation = Continuation::Codex {
            client: Box::new(client.with_reported(reported_model)),
            items: items
                .into_iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
                .collect(),
        };
        self
    }

    pub fn model_key(&self) -> Option<&ModelKey> {
        match &self.continuation {
            Continuation::Portable { model } => model.as_deref(),
            Continuation::Codex { client, .. } | Continuation::OpenRouter { client, .. } => {
                Some(client)
            }
        }
    }

    pub(crate) fn has_opaque_reasoning(&self) -> bool {
        match &self.continuation {
            Continuation::Portable { .. } => false,
            Continuation::Codex { items, .. } => !items.is_empty(),
            Continuation::OpenRouter { state, .. } => state.has_reasoning(),
        }
    }

    pub fn opaque_reasoning(&self) -> Result<Vec<OpaqueReasoning>, serde_json::Error> {
        match &self.continuation {
            Continuation::Portable { .. } => Ok(Vec::new()),
            Continuation::Codex { items, .. } => {
                items.iter().map(OpaqueReasoning::from_value).collect()
            }
            Continuation::OpenRouter { state, .. } => state.opaque_reasoning(),
        }
    }

    pub(crate) fn without_opaque(&self) -> Self {
        Self::new(self.content.clone(), self.tool_calls.clone(), self.usage)
    }

    pub(crate) fn with_openrouter_continuation(
        mut self,
        client: ModelKey,
        state: crate::openrouter::Replay,
    ) -> Self {
        self.continuation = Continuation::OpenRouter {
            client: Box::new(client),
            state,
        };
        self
    }

    pub(crate) fn openrouter_replay(&self) -> Option<&crate::openrouter::Replay> {
        match &self.continuation {
            Continuation::OpenRouter { state, .. } => Some(state),
            Continuation::Portable { .. } | Continuation::Codex { .. } => None,
        }
    }

    pub(crate) fn codex_items(&self) -> Option<&[Value]> {
        match &self.continuation {
            Continuation::Portable { .. } | Continuation::OpenRouter { .. } => None,
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
    stable_prefix: Option<usize>,
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

    #[must_use]
    pub const fn with_stable_prefix(mut self, messages: usize) -> Self {
        self.stable_prefix = Some(messages);
        self
    }

    #[must_use]
    pub const fn stable_prefix(&self) -> Option<usize> {
        self.stable_prefix
    }
}

pub trait ChatModel: Send + Sync {
    fn model_key(&self) -> ModelKey {
        ModelKey::new(Backend::OpenAiCompatible, "portable")
    }

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

#[cfg(test)]
mod durable_key_tests {
    use super::*;

    #[test]
    fn opaque_deserialization_uses_the_compact_constructor() {
        let bytes: OpaqueReasoning = serde_json::from_str("{\n  \"text\":\"a\\nb\"\n}").unwrap();
        assert_eq!(bytes.bytes(), br#"{"text":"a\nb"}"#);
        assert!(!bytes.bytes().contains(&b'\n'));
    }

    #[test]
    fn model_keys_survive_serialization_and_distinguish_reported_models() {
        let key = ModelKey::new(Backend::Codex, "upstream").with_reported(Some("reported".into()));
        let restored: ModelKey =
            serde_json::from_slice(&serde_json::to_vec(&key).unwrap()).unwrap();
        assert_eq!(restored, key);
        assert_ne!(
            restored,
            ModelKey::new(Backend::Codex, "upstream").with_reported(Some("other".into()))
        );
        assert_ne!(
            restored,
            ModelKey::new(Backend::OpenRouter, "upstream").with_reported(Some("reported".into()))
        );
    }

    #[test]
    fn opaque_capture_is_compact_and_never_debugs_its_body() {
        let value = serde_json::json!({"type":"reasoning", "encrypted_content":"opaque-canary\nsecond-line"});
        let opaque = OpaqueReasoning::from_value(&value).unwrap();
        assert!(!opaque.bytes().contains(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<Value>(opaque.bytes()).unwrap(),
            value
        );
        assert!(!format!("{opaque:?}").contains("opaque-canary"));
    }
}
