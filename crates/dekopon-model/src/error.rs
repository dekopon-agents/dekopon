use thiserror::Error;

use crate::{asset::BlobError, chatgpt::ChatGptError, model::sanitize_diagnostic};

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum ProviderCode {
    Text(String),
    Number(serde_json::Number),
}
impl ProviderCode {
    pub(crate) fn sanitized(&self, secrets: crate::diagnostic::DiagnosticSecrets<'_>) -> String {
        match self {
            Self::Text(value) => secrets.sanitize(value),
            Self::Number(value) => secrets.sanitize(&value.to_string()),
        }
    }
}

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("invalid model request: {0}")]
    InvalidRequest(#[from] RequestError),
    #[error("unsupported model feature: {0}")]
    Unsupported(#[from] UnsupportedFeature),
    #[error("model request failed: {0}")]
    Provider(#[from] ProviderFailure),
    #[error("model transport failed: {0}")]
    Transport(#[from] TransportFailure),
    #[error("invalid model response: {0}")]
    Protocol(#[from] ProtocolFailure),
    #[error("{0}")]
    Attachment(#[from] BlobError),
    #[error("model authentication failed: {0}")]
    Authentication(#[from] AuthError),
    #[error("model rate limited: {0}")]
    RateLimited(#[from] RateLimitError),
    #[error("model turn deadline exceeded")]
    DeadlineExceeded,
    #[error("model turn interrupted by its caller")]
    Cancelled,
}

#[derive(Debug, Error)]
pub enum RequestError {
    #[error("generation override requires http://127.0.0.1 or http://[::1]")]
    InvalidLoopbackEndpoint,
    #[error("OpenRouter credential must not be blank")]
    EmptyCredential,
    #[error("explicitPrefix requires a leading system message")]
    CacheAnchor,
    #[error("invalid OpenRouter setting: {0}")]
    OpenRouterSetting(crate::openrouter::settings::SettingsProblem),
    #[error("model request body cannot be replayed")]
    NonReplayableBody,
    #[error("model deadline exceeds the clock range")]
    DeadlineOutOfRange,
    #[error("model timeout must be greater than zero")]
    ZeroTimeout,
    #[error("model endpoint must not be empty")]
    EmptyEndpoint,
    #[error("model name must not be empty")]
    EmptyModel,
    #[error("bearer tokens require HTTPS or a loopback HTTP endpoint")]
    InsecureBearer,
}

#[derive(Debug, Error)]
pub enum UnsupportedFeature {
    #[error("model returned unsupported tool kind {0:?}")]
    ToolKind(String),
}

#[derive(Debug, Error)]
pub struct ProviderFailure {
    pub phase: FailurePhase,
    pub code: Option<String>,
    pub request_id: Option<String>,
    pub retry_after: Option<std::time::Duration>,
    /// Response status; a failure event in an HTTP-200 stream retains 200.
    pub status: Option<u16>,
    pub diagnostic: String,
    #[source]
    source: Option<Box<ChatGptError>>,
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(formatter, "HTTP {status}: {}", self.diagnostic),
            None => write!(formatter, "HTTP: {}", self.diagnostic),
        }
    }
}

impl ProviderFailure {
    pub(crate) fn new(status: u16, diagnostic: &str) -> Self {
        Self {
            phase: FailurePhase::ReadingBody,
            code: None,
            request_id: None,
            retry_after: None,
            status: Some(status),
            diagnostic: sanitize_diagnostic(diagnostic),
            source: None,
        }
    }

    pub(crate) fn credential(status: u16, source: ChatGptError) -> Self {
        Self {
            phase: FailurePhase::BeforeSend,
            code: None,
            request_id: None,
            retry_after: None,
            status: Some(status),
            diagnostic: sanitize_diagnostic(&source.to_string()),
            source: Some(Box::new(source)),
        }
    }
}

#[derive(Debug, Error)]
pub enum TransportFailure {
    #[error("{context}")]
    Http {
        context: ProviderFailure,
        /// The client failure with its URL removed.
        #[source]
        source: reqwest::Error,
    },
    #[error("blocking model task failed: {0}")]
    Blocking(#[source] tokio::task::JoinError),
    #[error("request body: {0}")]
    Encoding(#[source] serde_json::Error),
    #[error("could not prepare model credential: {0}")]
    Credential(#[source] ChatGptError),
}

#[derive(Debug, Error)]
pub enum ProtocolFailure {
    #[error("invalid OpenRouter response: {0}")]
    OpenRouter(String),
    #[error("invalid JSON: {0}")]
    Decode(#[source] serde_json::Error),
    #[error(
        "buffered model response exceeded {} bytes",
        crate::http::MAX_BUFFERED_BYTES
    )]
    BufferedTooLarge,
    #[error("model response contained no choices")]
    NoChoices,
    #[error(
        "streaming was requested but the endpoint answered with {0}; write `stream: false` on this model for an endpoint that ignores `stream: true`"
    )]
    UnexpectedContentType(String),
    #[error("incomplete function call")]
    IncompleteToolCall,
    #[error("tool arguments for {name} must be a JSON string or object")]
    InvalidToolArguments { name: String },
    #[error("stream ended before [DONE] or a finish reason")]
    MissingTerminal,
    #[error(
        "model response stream exceeded {} bytes",
        crate::sse::MAX_STREAM_BYTES
    )]
    StreamTooLarge,
    #[error("invalid Responses event: {0}")]
    InvalidEvent(String),
    #[error("native continuation belongs to another configured client")]
    ContinuationMismatch,
    #[error("invalid event-stream UTF-8: {0}")]
    Utf8(#[source] std::str::Utf8Error),
}

#[derive(Clone, Copy, Debug)]
pub enum FailurePhase {
    BeforeSend,
    AwaitingHeaders,
    ReadingBody,
}

impl FailurePhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeSend => "before-send",
            Self::AwaitingHeaders => "awaiting-headers",
            Self::ReadingBody => "reading-body",
        }
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("{0}")]
    Credential(#[source] ChatGptError),
    #[error("{0}")]
    Provider(#[source] ProviderFailure),
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct RateLimitError(pub ProviderFailure);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_failure_display_omits_option_debug_wrappers() {
        let mut failure = ProviderFailure::new(401, "refused");
        assert_eq!(failure.to_string(), "HTTP 401: refused");
        failure.status = None;
        assert_eq!(failure.to_string(), "HTTP: refused");
    }
}
