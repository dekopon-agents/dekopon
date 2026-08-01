use std::time::Duration;

use serde::{Deserialize, Serialize};
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

/// One model-request conversation message.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
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

    /// Creates a tool result message.
    #[must_use]
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool",
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            replay_items: Vec::new(),
        }
    }

    fn assistant(turn: &AssistantTurn) -> Self {
        Self {
            role: "assistant",
            content: turn.content.clone(),
            tool_calls: turn.tool_calls.clone(),
            tool_call_id: None,
            replay_items: turn.replay_items.clone(),
        }
    }

    fn plain(role: &'static str, content: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(content.into()),
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

    /// Returns message content when present.
    #[must_use]
    pub fn content(&self) -> Option<&str> {
        self.content.as_deref()
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

/// One assistant response, which may contain text or tool calls.
#[derive(Clone, Debug, PartialEq)]
pub struct AssistantTurn {
    /// Assistant text, if any.
    pub content: Option<String>,
    /// Requested tool calls.
    pub tool_calls: Vec<ModelToolCall>,
    /// Provider-specific opaque response items required for safe replay.
    #[doc(hidden)]
    pub replay_items: Vec<Value>,
}

/// Synchronous model boundary used by the immediate prompt loop.
pub trait ChatModel {
    /// Requests the next assistant turn.
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError>;
}

/// OpenAI-compatible chat-completions client.
pub struct OpenAiChatModel {
    agent: Agent,
    endpoint: String,
    model: String,
    bearer_token: Option<String>,
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

        let config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .build();
        let agent = config.into();
        let bearer_token = bearer_token.and_then(|token| {
            let token = token.trim().to_owned();
            (!token.is_empty()).then_some(token)
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
        let request_body = ChatRequest {
            model: &self.model,
            messages,
            tools: &tools,
            tool_choice: "auto",
        };

        let mut request = self
            .agent
            .post(&self.endpoint)
            .header("accept", "application/json");
        if let Some(token) = &self.bearer_token {
            request = request.header("authorization", &format!("Bearer {token}"));
        }
        let mut response = request
            .send_json(&request_body)
            .map_err(|error| ModelError::Request(error.to_string()))?;
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
            replay_items: Vec::new(),
        })
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ModelMessage],
    tools: &'a [OpenAiTool<'a>],
    tool_choice: &'static str,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::time::Duration;

    use super::{
        ModelError, ModelToolCall, OpenAiChatModel, WireFunctionCall, WireToolCall, completion_url,
    };

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
}
