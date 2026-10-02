use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use dekopon_model::wire::{ChatUsage, MessagesUsage, ResponsesUsage};
use dekopon_model_token_governor::{ModelUsage, Refusal};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Dialect {
    Messages,
    CountTokens,
    Responses,
    Chat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Problem {
    Invalid,
    Forbidden,
    NotFound,
    TooLarge,
    Throttled,
    Upstream,
}

impl Problem {
    const fn status(self) -> StatusCode {
        match self {
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Throttled => StatusCode::TOO_MANY_REQUESTS,
            Self::Upstream => StatusCode::BAD_GATEWAY,
        }
    }

    const fn anthropic_type(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_request_error",
            Self::Forbidden => "permission_error",
            Self::NotFound => "not_found_error",
            Self::TooLarge => "request_too_large",
            Self::Throttled => "rate_limit_error",
            Self::Upstream => "api_error",
        }
    }

    const fn openai_type_and_code(self) -> (&'static str, &'static str) {
        match self {
            Self::Invalid => ("invalid_request_error", "invalid_request"),
            Self::Forbidden => ("invalid_request_error", "permission_denied"),
            Self::NotFound => ("invalid_request_error", "model_not_found"),
            Self::TooLarge => ("invalid_request_error", "request_too_large"),
            Self::Throttled => ("tokens", "rate_limit_exceeded"),
            Self::Upstream => ("server_error", "upstream_unavailable"),
        }
    }
}

impl Dialect {
    pub(crate) fn error(self, problem: Problem, message: &str) -> Response {
        let body = match self {
            Self::Messages | Self::CountTokens => json!({
                "type": "error",
                "error": {"type": problem.anthropic_type(), "message": message},
            }),
            Self::Responses | Self::Chat => {
                let (kind, code) = problem.openai_type_and_code();
                json!({
                    "error": {"message": message, "type": kind, "param": null, "code": code},
                })
            }
        };
        json_response(problem.status(), &body)
    }

    /// A wait becomes the dialect's throttling error with `retry-after`; a request that can never
    /// fit is a plain invalid request, never worded as "prompt is too long", which Claude Code
    /// reads as a compaction trigger.
    pub(crate) fn refusal(self, refusal: &Refusal) -> Response {
        let message = refusal.for_guest().to_string();
        match refusal.retry_after() {
            Some(wait) => {
                let mut response = self.error(Problem::Throttled, &message);
                let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, value);
                }
                response
            }
            None => self.error(Problem::Invalid, &message),
        }
    }

    pub(crate) fn observe(self, event: &Value, observed: &mut Observation) {
        match self {
            Self::Messages | Self::CountTokens => observe_messages(event, observed),
            Self::Responses => observe_responses(event, observed),
            Self::Chat => observe_chat(event, observed),
        }
    }

    pub(crate) fn observe_body(self, body: &Value, observed: &mut Observation) {
        let Some(usage) = body.get("usage").filter(|usage| usage.is_object()) else {
            return;
        };
        observed.usage = match self {
            Self::Messages | Self::CountTokens => usage_from::<MessagesUsage>(usage),
            Self::Responses => usage_from::<ResponsesUsage>(usage),
            Self::Chat => serde_json::from_value::<ChatUsage<u64>>(usage.clone())
                .ok()
                .map(ChatUsage::into_openrouter),
        };
    }
}

#[derive(Debug, Default)]
pub(crate) struct Observation {
    pub usage: Option<ModelUsage>,
    pub text_bytes: usize,
}

fn usage_from<T: serde::de::DeserializeOwned + Into<ModelUsage>>(
    usage: &Value,
) -> Option<ModelUsage> {
    serde_json::from_value::<T>(usage.clone())
        .ok()
        .map(Into::into)
}

fn text_len(value: Option<&Value>) -> usize {
    value.and_then(Value::as_str).map_or(0, str::len)
}

fn observe_messages(event: &Value, observed: &mut Observation) {
    match event.get("type").and_then(Value::as_str) {
        // message_start's output count is a placeholder; message_delta reports the real one.
        Some("message_start") => {
            observed.usage = event
                .pointer("/message/usage")
                .and_then(usage_from::<MessagesUsage>)
                .map(|usage| ModelUsage {
                    output_tokens: None,
                    total_tokens: None,
                    ..usage
                });
        }
        Some("message_delta") => {
            observed.usage = event.get("usage").and_then(usage_from::<MessagesUsage>);
        }
        Some("content_block_delta") => {
            let delta = event.get("delta");
            observed.text_bytes += ["text", "thinking", "partial_json"]
                .into_iter()
                .map(|field| text_len(delta.and_then(|delta| delta.get(field))))
                .sum::<usize>();
        }
        Some(_) | None => {}
    }
}

fn observe_responses(event: &Value, observed: &mut Observation) {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind.ends_with(".delta") {
        observed.text_bytes += text_len(event.get("delta"));
    }
    if matches!(
        kind,
        "response.completed" | "response.incomplete" | "response.failed"
    ) && let Some(usage) = event
        .pointer("/response/usage")
        .filter(|usage| usage.is_object())
    {
        observed.usage = usage_from::<ResponsesUsage>(usage);
    }
}

fn observe_chat(event: &Value, observed: &mut Observation) {
    for choice in event
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let delta = choice.get("delta");
        observed.text_bytes += ["content", "reasoning"]
            .into_iter()
            .map(|field| text_len(delta.and_then(|delta| delta.get(field))))
            .sum::<usize>();
    }
    if let Some(usage) = event.get("usage").filter(|usage| usage.is_object()) {
        observed.usage = serde_json::from_value::<ChatUsage<u64>>(usage.clone())
            .ok()
            .map(ChatUsage::into_openrouter);
    }
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}
