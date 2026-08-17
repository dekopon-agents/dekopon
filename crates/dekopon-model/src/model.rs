use std::time::Duration;

use dekopon_core::Redacted;

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

        let config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .build();
        let agent = config.into();
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
            // One of the few places a credential leaves its wrapper, and it goes straight onto the
            // wire rather than into a variable that could later be formatted somewhere else.
            request = request.header("authorization", &format!("Bearer {}", token.expose()));
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
            usage: response.usage.map(ModelUsage::from),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use std::time::Duration;

    use serde_json::Value;

    use super::{
        AssistantTurn, ChatRequest, ChatResponse, ModelError, ModelFunctionCall, ModelMessage,
        ModelTool, ModelToolCall, ModelUsage, OpenAiChatModel, OpenAiTool, WireFunctionCall,
        WireToolCall, assistant_message, completion_url,
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
                    messages: &messages,
                    tools: &tools,
                    tool_choice: "auto",
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
