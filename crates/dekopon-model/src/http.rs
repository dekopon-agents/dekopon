//! Shared generation HTTP policy, cancellation and telemetry.

use crate::{
    control::TurnControl,
    diagnostic::DiagnosticSecrets,
    error::{
        AuthError, FailurePhase, InferenceError, ProtocolFailure, ProviderFailure, RateLimitError,
        RequestError, TransportFailure,
    },
    model::{AssistantTurn, MAX_ERROR_BODY_BYTES},
    sse::{SseEvent, read_async_stream},
    stream::TurnEvent,
};
use std::{
    future::Future,
    ops::ControlFlow,
    time::{Duration, Instant},
};
use tracing::Instrument as _;

// Preserve the synchronous client's former JSON-reader default, independently of SSE's ceiling.
pub(crate) const MAX_BUFFERED_BYTES: usize = 10 * 1024 * 1024;

pub(crate) fn record_phase(phase: FailurePhase) {
    tracing::Span::current().record("error.phase", phase.as_str());
}

pub(crate) struct InferenceHttp {
    client: reqwest::Client,
}

impl InferenceHttp {
    pub(crate) fn new(timeout: Duration) -> Result<Self, InferenceError> {
        if timeout.is_zero() {
            return Err(RequestError::ZeroTimeout.into());
        }
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .build()
            .map_err(|source| http_failure(FailurePhase::BeforeSend, None, source))?;
        Ok(Self { client })
    }

    pub(crate) fn post(&self, endpoint: &str) -> reqwest::RequestBuilder {
        self.client.post(endpoint)
    }

    pub(crate) async fn send(
        &self,
        request: reqwest::RequestBuilder,
        control: &TurnControl,
        secrets: DiagnosticSecrets<'_>,
    ) -> Result<InferenceResponse, InferenceError> {
        let started = Instant::now();
        let response = control
            .run(request.send())
            .await
            .inspect_err(|_| record_phase(FailurePhase::AwaitingHeaders))?
            .map_err(|source| http_failure(FailurePhase::AwaitingHeaders, None, source))?;
        tracing::Span::current().record("timing.headers_ms", millis(started.elapsed()));
        let mut context = ProviderFailure::new(response.status().as_u16(), "");
        context.phase = FailurePhase::AwaitingHeaders;
        context.request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("request-id"))
            .and_then(|value| value.to_str().ok())
            .map(|value| secrets.sanitize(value));
        let span = tracing::Span::current();
        span.record("http.status", response.status().as_u16());
        if let Some(request_id) = &context.request_id {
            span.record("provider.request_id", request_id);
        }
        context.retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .map(Duration::from_secs);
        Ok(InferenceResponse { response, context })
    }

    pub(crate) async fn generation(
        &self,
        name: &str,
        model: &str,
        provider: &str,
        dialect: &str,
        operation: impl Future<Output = Result<AssistantTurn, InferenceError>> + Send,
    ) -> Result<AssistantTurn, InferenceError> {
        let span = tracing::info_span!(
            "model.complete",
            model = model,
            model.name = name,
            model.backend = provider,
            model.dialect = dialect,
            model.stream = true,
            cache.style = "automatic",
            model.returned = tracing::field::Empty,
            model.upstream = tracing::field::Empty,
            timing.total_ms = tracing::field::Empty,
            timing.headers_ms = tracing::field::Empty,
            timing.first_event_ms = tracing::field::Empty,
            stream.first_delta_ms = tracing::field::Empty,
            stream.deltas = tracing::field::Empty,
            response.bytes = tracing::field::Empty,
            tool_call.count = tracing::field::Empty,
            usage.input_tokens = tracing::field::Empty,
            usage.cached_input_tokens = tracing::field::Empty,
            usage.cache_write_tokens = tracing::field::Empty,
            generation.max_output_tokens = tracing::field::Empty,
            generation.temperature = tracing::field::Empty,
            generation.top_p = tracing::field::Empty,
            reasoning.effort = tracing::field::Empty,
            routing.allow_fallbacks = tracing::field::Empty,
            routing.require_parameters = tracing::field::Empty,
            routing.only = tracing::field::Empty,
            cache.ttl = tracing::field::Empty,
            usage.output_tokens = tracing::field::Empty,
            usage.reasoning_output_tokens = tracing::field::Empty,
            usage.total_tokens = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error = tracing::field::Empty,
            error.kind = tracing::field::Empty,
            error.phase = tracing::field::Empty,
            http.status = tracing::field::Empty,
            provider.code = tracing::field::Empty,
            provider.request_id = tracing::field::Empty,
            finish.reason = tracing::field::Empty,
            output.partial = false,
        );
        let started = Instant::now();
        let result = operation.instrument(span.clone()).await;
        span.record("timing.total_ms", millis(started.elapsed()));
        match &result {
            Ok(turn) => {
                span.record("outcome", "success");
                span.record("tool_call.count", turn.tool_calls.len());
                if let Some(usage) = turn.usage {
                    for (key, value) in [
                        ("usage.input_tokens", usage.input_tokens),
                        ("usage.cached_input_tokens", usage.cached_input_tokens),
                        ("usage.cache_write_tokens", usage.cache_write_tokens),
                        ("usage.output_tokens", usage.output_tokens),
                        (
                            "usage.reasoning_output_tokens",
                            usage.reasoning_output_tokens,
                        ),
                        ("usage.total_tokens", usage.total_tokens),
                    ] {
                        if let Some(value) = value {
                            span.record(key, value);
                        }
                    }
                }
            }
            Err(error) => {
                let (kind, context) = match error {
                    InferenceError::Authentication(AuthError::Provider(context)) => {
                        ("authentication", Some(context))
                    }
                    InferenceError::Authentication(_) => ("authentication", None),
                    InferenceError::RateLimited(RateLimitError(context)) => {
                        ("rate-limited", Some(context))
                    }
                    InferenceError::Provider(context) => ("provider", Some(context)),
                    InferenceError::Transport(TransportFailure::Http { context, .. }) => {
                        ("transport", Some(context))
                    }
                    InferenceError::Transport(_) => ("transport", None),
                    InferenceError::Protocol(_) => ("protocol", None),
                    InferenceError::Attachment(_) => ("attachment", None),
                    InferenceError::InvalidRequest(_) => ("invalid-request", None),
                    InferenceError::Unsupported(_) => ("unsupported", None),
                    InferenceError::Cancelled => ("cancelled", None),
                    InferenceError::DeadlineExceeded => ("deadline-exceeded", None),
                };
                span.record("outcome", "failed");
                // Upstream messages and parser sources may echo sensitive request data.
                span.record("error", kind);
                span.record("error.kind", kind);
                if matches!(
                    error,
                    InferenceError::InvalidRequest(_)
                        | InferenceError::Attachment(_)
                        | InferenceError::Authentication(AuthError::Credential(_))
                        | InferenceError::Transport(
                            TransportFailure::Encoding(_)
                                | TransportFailure::Blocking(_)
                                | TransportFailure::Credential(_)
                        )
                ) {
                    span.record("error.phase", FailurePhase::BeforeSend.as_str());
                }
                if let Some(context) = context {
                    span.record("error.phase", context.phase.as_str());
                    if let Some(status) = context.status {
                        span.record("http.status", status);
                    }
                    if let Some(code) = &context.code {
                        span.record("provider.code", code);
                    }
                    if let Some(id) = &context.request_id {
                        span.record("provider.request_id", id);
                    }
                }
            }
        }
        result
    }
}

pub(crate) struct InferenceResponse {
    response: reqwest::Response,
    context: ProviderFailure,
}

impl InferenceResponse {
    pub(crate) fn status(&self) -> u16 {
        self.response.status().as_u16()
    }

    pub(crate) fn require_sse(&self, secrets: DiagnosticSecrets<'_>) -> Result<(), InferenceError> {
        let content_type = self
            .response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if !content_type
            .to_ascii_lowercase()
            .starts_with("text/event-stream")
        {
            let shown = if content_type.is_empty() {
                "no content type".to_owned()
            } else {
                format!("`{}`", secrets.sanitize(content_type))
            };
            record_phase(FailurePhase::ReadingBody);
            return Err(ProtocolFailure::UnexpectedContentType(shown).into());
        }
        Ok(())
    }

    pub(crate) async fn buffered(
        mut self,
        control: &TurnControl,
        decode: impl FnOnce(&[u8]) -> Result<AssistantTurn, InferenceError> + Send,
    ) -> Result<AssistantTurn, InferenceError> {
        let mut bytes = Vec::new();
        loop {
            let chunk = match control
                .run(self.response.chunk())
                .await
                .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?
            {
                Ok(chunk) => chunk,
                Err(source) => {
                    return Err(response_context(
                        http_failure(FailurePhase::ReadingBody, self.context.status, source),
                        self.context,
                    ));
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            if chunk.len() > MAX_BUFFERED_BYTES - bytes.len() {
                record_phase(FailurePhase::ReadingBody);
                return Err(ProtocolFailure::BufferedTooLarge.into());
            }
            bytes.extend_from_slice(&chunk);
            tracing::Span::current().record("response.bytes", bytes.len());
        }
        control
            .check()
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        decode(&bytes)
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))
            .map_err(|error| response_context(error, self.context))
    }

    pub(crate) async fn check(
        mut self,
        control: &TurnControl,
        secrets: DiagnosticSecrets<'_>,
    ) -> Result<Self, InferenceError> {
        if self.response.status().is_success() {
            return Ok(self);
        }
        let mut bytes = Vec::new();
        tracing::Span::current().record("response.bytes", 0);
        let limit = MAX_ERROR_BODY_BYTES as usize + 1;
        while bytes.len() < limit {
            let chunk = control
                .run(self.response.chunk())
                .await
                .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(source) => {
                    self.context.phase = FailurePhase::ReadingBody;
                    let source = source.without_url();
                    self.context.diagnostic = secrets.sanitize(&source.to_string());
                    return Err(TransportFailure::Http {
                        context: self.context,
                        source,
                    }
                    .into());
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            bytes.extend_from_slice(&chunk[..chunk.len().min(limit - bytes.len())]);
            tracing::Span::current().record("response.bytes", bytes.len());
        }
        let truncated = bytes.len() == limit;
        self.context.phase = FailurePhase::ReadingBody;
        self.context.diagnostic = if truncated {
            secrets.sanitize_truncated(&bytes)
        } else {
            secrets.sanitize(&String::from_utf8_lossy(&bytes))
        };
        #[derive(serde::Deserialize)]
        struct Envelope {
            error: Detail,
        }
        #[derive(serde::Deserialize)]
        struct Detail {
            code: Option<crate::error::ProviderCode>,
            message: Option<String>,
        }
        if !truncated && let Ok(detail) = serde_json::from_slice::<Envelope>(&bytes) {
            self.context.code = detail
                .error
                .code
                .as_ref()
                .map(|value| value.sanitized(secrets));
            if let Some(message) = detail.error.message {
                self.context.diagnostic = secrets.sanitize(&message);
            }
        }
        Err(match self.status() {
            401 | 403 => AuthError::Provider(self.context).into(),
            429 => RateLimitError(self.context).into(),
            _ => self.context.into(),
        })
    }

    pub(crate) async fn sse(
        self,
        control: &TurnControl,
        on_event: &mut (impl FnMut(SseEvent<'_>) -> Result<ControlFlow<()>, InferenceError> + Send),
    ) -> Result<(), InferenceError> {
        read_async_stream(self.response.bytes_stream(), control, on_event)
            .await
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))
            .map_err(|error| response_context(error, self.context))
    }
}

fn response_context(error: InferenceError, metadata: ProviderFailure) -> InferenceError {
    match error {
        InferenceError::Provider(mut context) => {
            context.status = metadata.status;
            context.request_id = metadata.request_id;
            context.retry_after = metadata.retry_after;
            context.into()
        }
        InferenceError::Transport(TransportFailure::Http {
            mut context,
            source,
        }) => {
            context.status = metadata.status;
            context.request_id = metadata.request_id;
            context.retry_after = metadata.retry_after;
            TransportFailure::Http { context, source }.into()
        }
        other => other,
    }
}

pub(crate) fn http_failure(
    phase: FailurePhase,
    status: Option<u16>,
    source: reqwest::Error,
) -> InferenceError {
    let source = source.without_url();
    let mut context = ProviderFailure::new(status.unwrap_or(0), &source.to_string());
    context.status = status;
    context.phase = phase;
    TransportFailure::Http { context, source }.into()
}

pub(crate) struct Progress {
    started: Instant,
    events: u64,
    deltas: u64,
}

impl Progress {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            events: 0,
            deltas: 0,
        }
    }
    pub(crate) fn event(&mut self) {
        if self.events == 0 {
            tracing::Span::current()
                .record("timing.first_event_ms", millis(self.started.elapsed()));
        }
        self.events += 1;
    }
    pub(crate) fn observe(
        &mut self,
        event: TurnEvent,
        observer: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> ControlFlow<()> {
        if matches!(event, TurnEvent::TextDelta(_)) {
            if self.deltas == 0 {
                tracing::Span::current()
                    .record("stream.first_delta_ms", millis(self.started.elapsed()));
            }
            self.deltas += 1;
            tracing::Span::current().record("stream.deltas", self.deltas);
            tracing::Span::current().record("output.partial", true);
        }
        observer(event)
    }
}
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockResponse, MockServer};

    async fn refusal_with_body(bytes: usize) -> ProviderFailure {
        let server = MockServer::start(vec![MockResponse::failure(
            500,
            serde_json::Value::String("x".repeat(bytes - 2)),
        )]);
        let http = InferenceHttp::new(Duration::from_secs(2)).unwrap();
        let control =
            TurnControl::new(tokio::sync::watch::channel(false).1, Duration::from_secs(2)).unwrap();
        use tracing::instrument::WithSubscriber as _;
        let capture = crate::trace_capture::TraceCapture::default();
        let result = http
            .generation("refusal", "fixture", "fixture", "fixture", async {
                let response = http
                    .send(
                        http.post(&server.base_url()).body("{}"),
                        &control,
                        DiagnosticSecrets::default(),
                    )
                    .await?;
                match response.check(&control, DiagnosticSecrets::default()).await {
                    Err(error) => Err(error),
                    Ok(_) => panic!("expected refusal"),
                }
            })
            .with_subscriber(capture.subscriber())
            .await;
        let Err(InferenceError::Provider(error)) = result else {
            panic!("expected provider refusal");
        };
        let recorded = bytes.min(MAX_ERROR_BODY_BYTES as usize + 1);
        assert_eq!(capture.field("response.bytes"), Some(recorded.to_string()));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        error
    }

    #[tokio::test]
    async fn a_short_refusal_records_all_consumed_response_bytes() {
        let error = refusal_with_body(128).await;
        assert_eq!(error.diagnostic.len(), 128);
    }

    #[tokio::test]
    async fn a_long_error_body_is_truncated() {
        let error = refusal_with_body(2 * MAX_ERROR_BODY_BYTES as usize).await;
        assert_eq!(error.diagnostic.len(), MAX_ERROR_BODY_BYTES as usize);
    }
}
