//! Typed failures shared by model clients and the prompt loop.

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

/// Failure while preparing, requesting, or decoding one complete model turn.
#[derive(Debug, Error)]
pub enum InferenceError {
    /// The request cannot be sent as configured.
    #[error("invalid model request: {0}")]
    InvalidRequest(#[from] RequestError),
    /// The model requested a feature the loop cannot execute.
    #[error("unsupported model feature: {0}")]
    Unsupported(#[from] UnsupportedFeature),
    /// The provider refused or failed the request.
    #[error("model request failed: {0}")]
    Provider(#[from] ProviderFailure),
    /// Sending or reading failed without a provider refusal.
    #[error("model transport failed: {0}")]
    Transport(#[from] TransportFailure),
    /// The response cannot be reduced to a complete turn.
    #[error("invalid model response: {0}")]
    Protocol(#[from] ProtocolFailure),
    /// A retained attachment could not be read; no request was sent.
    #[error("{0}")]
    Attachment(#[from] BlobError),
    /// The credential or its authorization was refused.
    #[error("model authentication failed: {0}")]
    Authentication(#[from] AuthError),
    /// The provider reported a rate limit; no automatic retry occurs.
    #[error("model rate limited: {0}")]
    RateLimited(#[from] RateLimitError),
    /// The total turn deadline elapsed.
    #[error("model turn deadline exceeded")]
    DeadlineExceeded,
    /// The caller stopped the turn; no partial turn is returned.
    #[error("model turn interrupted by its caller")]
    Cancelled,
}

/// A local refusal before sending a generation request.
#[derive(Debug, Error)]
pub enum RequestError {
    /// Offline overrides must be plain HTTP at the exact literal loopback addresses.
    #[error("generation override requires http://127.0.0.1 or http://[::1]")]
    InvalidLoopbackEndpoint,
    /// OpenRouter requires a nonblank caller-supplied credential.
    #[error("OpenRouter credential must not be blank")]
    EmptyCredential,
    /// Explicit caching requires at least one leading system message.
    #[error("explicitPrefix requires a leading system message")]
    CacheAnchor,
    /// One authored setting is outside its supported range.
    #[error("invalid OpenRouter setting: {0}")]
    OpenRouterSetting(crate::openrouter::settings::SettingsProblem),
    /// A request body could not be retained for Codex's one permitted resend.
    #[error("model request body cannot be replayed")]
    NonReplayableBody,
    /// The requested duration exceeds the platform clock range.
    #[error("model deadline exceeds the clock range")]
    DeadlineOutOfRange,
    /// A deadline must have a positive duration.
    #[error("model timeout must be greater than zero")]
    ZeroTimeout,
    /// A compatible endpoint must be named.
    #[error("model endpoint must not be empty")]
    EmptyEndpoint,
    /// A model must be named.
    #[error("model name must not be empty")]
    EmptyModel,
    /// A bearer token must not cross remote plaintext HTTP.
    #[error("bearer tokens require HTTPS or a loopback HTTP endpoint")]
    InsecureBearer,
}

/// Model output the prompt loop cannot execute.
#[derive(Debug, Error)]
pub enum UnsupportedFeature {
    /// Only function tools are executable.
    #[error("model returned unsupported tool kind {0:?}")]
    ToolKind(String),
}

/// A provider refusal, retaining the status and a bounded diagnostic.
#[derive(Debug, Error)]
pub struct ProviderFailure {
    /// Where the failure occurred.
    pub phase: FailurePhase,
    /// Bounded upstream error code, if supplied.
    pub code: Option<String>,
    /// Bounded upstream request identifier, if supplied.
    pub request_id: Option<String>,
    /// Integer-seconds Retry-After, if supplied.
    pub retry_after: Option<std::time::Duration>,
    /// Response status; a failure event in an HTTP-200 stream retains 200.
    pub status: Option<u16>,
    /// Sanitized provider detail, bounded by the model error-body ceiling.
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

/// The underlying failure when no provider response refused the request.
#[derive(Debug, Error)]
pub enum TransportFailure {
    /// Async HTTP send/read failure with response context when available.
    #[error("{context}")]
    Http {
        /// Bounded metadata, not a successful provider response.
        context: ProviderFailure,
        /// The client failure with its URL removed.
        #[source]
        source: reqwest::Error,
    },
    /// A blocking credential or attachment task failed to join.
    #[error("blocking model task failed: {0}")]
    Blocking(#[source] tokio::task::JoinError),
    /// Serializing the request failed before a response existed.
    #[error("request body: {0}")]
    Encoding(#[source] serde_json::Error),
    /// Resolving or refreshing the model credential failed before generation.
    #[error("could not prepare model credential: {0}")]
    Credential(#[source] ChatGptError),
}

/// A response that cannot become a completed assistant turn.
#[derive(Debug, Error)]
pub enum ProtocolFailure {
    /// Invalid OpenRouter native state.
    #[error("invalid OpenRouter response: {0}")]
    OpenRouter(String),
    /// Malformed JSON, with its parser source preserved.
    #[error("invalid JSON: {0}")]
    Decode(#[source] serde_json::Error),
    /// The buffered response exceeded the existing JSON-body ceiling.
    #[error(
        "buffered model response exceeded {} bytes",
        crate::http::MAX_BUFFERED_BYTES
    )]
    BufferedTooLarge,
    /// A buffered completion contained no choices.
    #[error("model response contained no choices")]
    NoChoices,
    /// A streaming request received another content type.
    #[error(
        "streaming was requested but the endpoint answered with {0}; write `stream: false` on this model for an endpoint that ignores `stream: true`"
    )]
    UnexpectedContentType(String),
    /// A terminal tool call omitted its identifier or function name.
    #[error("incomplete function call")]
    IncompleteToolCall,
    /// Tool arguments were not a complete JSON object.
    #[error("tool arguments for {name} must be a JSON string or object")]
    InvalidToolArguments {
        /// The bounded model-selected function name.
        name: String,
    },
    /// EOF arrived without a dialect terminal marker.
    #[error("stream ended before [DONE] or a finish reason")]
    MissingTerminal,
    /// The shared SSE byte ceiling was exceeded.
    #[error(
        "model response stream exceeded {} bytes",
        crate::sse::MAX_STREAM_BYTES
    )]
    StreamTooLarge,
    /// The Responses reducer rejected an event's required semantics.
    #[error("invalid Responses event: {0}")]
    InvalidEvent(String),
    /// A native continuation belongs to another configured client.
    #[error("native continuation belongs to another configured client")]
    ContinuationMismatch,
    /// The stream contained invalid UTF-8.
    #[error("invalid event-stream UTF-8: {0}")]
    Utf8(#[source] std::str::Utf8Error),
}

/// Phase of one generation exchange.
#[derive(Clone, Copy, Debug)]
pub enum FailurePhase {
    /// Local preparation has not sent a request.
    BeforeSend,
    /// A request is awaiting response headers.
    AwaitingHeaders,
    /// Headers arrived and the body is being read.
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

/// Credential resolution or upstream authorization refusal.
#[derive(Debug, Error)]
pub enum AuthError {
    /// A credential file could not supply a usable credential.
    #[error("{0}")]
    Credential(#[source] ChatGptError),
    /// The upstream refused authorization.
    #[error("{0}")]
    Provider(#[source] ProviderFailure),
}

/// A provider rate-limit response, including its optional Retry-After.
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
