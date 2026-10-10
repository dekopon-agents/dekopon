#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "tests spawn freely; production sites carry their own expectation"
    )
)]

mod dialect;
mod tee;
pub mod tls;

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
    routing::post,
};
use bytes::Bytes;
use dekopon_core::{AgentId, Redacted};
use dekopon_model::{
    chatgpt::CredentialFile,
    wire::{Field, PeekError, RequestPeek},
};
use dekopon_model_token_governor::{Call, Estimate, Metering, Outcome, Sizes, Tokens, Via};
use tracing::Instrument as _;

use dialect::Problem;
pub use dialect::{Dialect, SANDBOX};
use tee::{Shape, Timing};

pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
pub const SUBJECT_HEADER: &str = "x-dekopon-vm-subject";
pub const SESSION_HEADER: &str = "x-dekopon-vm-session";
/// Under the jail's 90 s idle timer, so a reasoning model's silence never drops the stream.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);
pub const UPSTREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ERROR_BYTES: usize = 64 * 1024;
const PASSED_HEADERS: [&str; 3] = ["anthropic-version", "anthropic-beta", "x-request-id"];
const RETURNED_HEADERS: [&str; 5] = [
    "content-type",
    "x-request-id",
    "request-id",
    "cache-control",
    "retry-after",
];

pub const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com";
pub const OPENROUTER_ENDPOINT: &str = "https://openrouter.ai/api";
pub const CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Clone)]
pub enum Upstream {
    Codex {
        endpoint: String,
        credential: Arc<CredentialFile>,
    },
    OpenRouter {
        endpoint: String,
        api_key: Redacted<String>,
    },
    Anthropic {
        endpoint: String,
        api_key: Redacted<String>,
    },
}

impl Upstream {
    const fn serves(&self, dialect: Dialect) -> bool {
        match (self, dialect) {
            (Self::Anthropic { .. }, Dialect::Messages | Dialect::CountTokens)
            | (Self::Codex { .. }, Dialect::Responses)
            | (Self::OpenRouter { .. }, Dialect::Chat) => true,
            (Self::Anthropic { .. }, Dialect::Responses | Dialect::Chat)
            | (Self::Codex { .. }, Dialect::Messages | Dialect::CountTokens | Dialect::Chat)
            | (
                Self::OpenRouter { .. },
                Dialect::Messages | Dialect::CountTokens | Dialect::Responses,
            ) => false,
        }
    }

    const fn path(&self) -> &'static str {
        match self {
            Self::Anthropic { .. } => "/v1/messages",
            Self::Codex { .. } => "/v1/responses",
            Self::OpenRouter { .. } => "/v1/chat/completions",
        }
    }

    fn url(&self, dialect: Dialect) -> String {
        let (endpoint, path) = match (self, dialect) {
            (Self::Anthropic { endpoint, .. }, Dialect::CountTokens) => {
                (endpoint, "/v1/messages/count_tokens")
            }
            (Self::Anthropic { endpoint, .. }, _) => (endpoint, "/v1/messages"),
            (Self::Codex { endpoint, .. }, _) => (endpoint, "/responses"),
            (Self::OpenRouter { endpoint, .. }, _) => (endpoint, "/v1/chat/completions"),
        };
        format!("{}{path}", endpoint.trim_end_matches('/'))
    }
}

pub struct ProxyModel {
    pub name: String,
    pub wire_model: String,
    pub backend: &'static str,
    pub reserve: Tokens,
    pub upstream: Upstream,
}

pub struct Grant {
    pub agent: AgentId,
    pub models: BTreeSet<String>,
}

pub struct ModelProxy {
    models: HashMap<String, ProxyModel>,
    guests: HashMap<String, Grant>,
    metering: Arc<Metering>,
    http: reqwest::Client,
    timing: Timing,
}

#[derive(Debug, thiserror::Error)]
#[error("model proxy HTTP client could not be built")]
pub struct ClientError(#[source] reqwest::Error);

impl ModelProxy {
    pub fn new(
        models: Vec<ProxyModel>,
        guests: HashMap<String, Grant>,
        metering: Arc<Metering>,
    ) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .read_timeout(UPSTREAM_IDLE_TIMEOUT)
            .build()
            .map_err(ClientError)?;
        Ok(Self {
            models: models
                .into_iter()
                .map(|model| (model.name.clone(), model))
                .collect(),
            guests,
            metering,
            http,
            timing: Timing {
                ping: PING_INTERVAL,
                idle: UPSTREAM_IDLE_TIMEOUT,
            },
        })
    }

    #[cfg(test)]
    fn with_timing(mut self, ping: Duration, idle: Duration) -> Self {
        self.timing = Timing { ping, idle };
        self
    }

    /// Names only configured models, never anything from the request.
    fn model_rule(&self, grant: &Grant, dialect: Dialect) -> String {
        let granted = || grant.models.iter().filter_map(|name| self.models.get(name));
        let served = granted()
            .filter(|model| model.upstream.serves(dialect))
            .map(|model| format!("`{}`", model.name))
            .collect::<Vec<_>>();
        if served.is_empty() {
            let elsewhere = granted()
                .map(|model| format!("`{}` on `{}`", model.name, model.upstream.path()))
                .collect::<Vec<_>>();
            format!(
                "this agent may only call its configured models, and none is served on this path: {}. Call one on its own path.",
                elsewhere.join(", ")
            )
        } else {
            format!(
                "this agent may only call its configured models: {}. Set the model to one of them.",
                served.join(", ")
            )
        }
    }

    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(
                "/v1/messages",
                post(|state, headers, body| handle(state, Dialect::Messages, headers, body)),
            )
            .route(
                "/v1/messages/count_tokens",
                post(|state, headers, body| handle(state, Dialect::CountTokens, headers, body)),
            )
            .route(
                "/v1/responses",
                post(|state, headers, body| handle(state, Dialect::Responses, headers, body)),
            )
            .route(
                "/v1/chat/completions",
                post(|state, headers, body| handle(state, Dialect::Chat, headers, body)),
            )
            .with_state(self)
    }
}

async fn handle(
    State(proxy): State<Arc<ModelProxy>>,
    dialect: Dialect,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let session = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            value.len() <= 128 && value.bytes().all(|byte| (b' '..=b'~').contains(&byte))
        })
        .unwrap_or_default();
    let span = if dialect == Dialect::CountTokens {
        tracing::Span::none()
    } else {
        tracing::info_span!(
            "model.proxy.call",
            vm.subject = headers
                .get(SUBJECT_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default(),
            vm.session = session,
            agent = "",
            model.name = "",
            usage.input_tokens = 0_u64,
            usage.output_tokens = 0_u64,
            outcome = Outcome::Failed.as_str(),
        )
    };
    let context = span.clone();
    async move {
        let Some(grant) = headers
            .get(SUBJECT_HEADER)
            .and_then(|subject| subject.to_str().ok())
            .and_then(|subject| proxy.guests.get(subject))
        else {
            return dialect.sandbox(
                Problem::Forbidden,
                "this VM is granted no model. Ask the operator to grant its agent one.",
            );
        };
        span.record("agent", grant.agent.as_str());
        let body = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                tracing::debug!(target: "model", error = %error, "proxy request body refused");
                return dialect.sandbox(
                    Problem::TooLarge,
                    "a request body may be at most 8 MiB. Send a smaller request.",
                );
            }
        };
        let peek = match RequestPeek::of(&body) {
            Ok(peek) => peek,
            Err(error) => return dialect.sandbox(Problem::Invalid, &peek_rule(&error, grant)),
        };
        if let Some(model) = proxy.models.get(&peek.model) {
            span.record("model.name", model.name.as_str());
        }
        let Some(model) = proxy
            .models
            .get(&peek.model)
            .filter(|model| grant.models.contains(&model.name) && model.upstream.serves(dialect))
        else {
            return dialect.sandbox(Problem::Forbidden, &proxy.model_rule(grant, dialect));
        };
        let wire_model = serde_json::Value::from(model.wire_model.as_str()).to_string();
        let rewritten = match &model.upstream {
            Upstream::Codex { .. } if peek.stream != Some(true) => {
                return dialect.sandbox(
                    Problem::Invalid,
                    "this model is served only as a stream. Set `stream` to true.",
                );
            }
            Upstream::Codex { .. } => peek.rewrite(
                &body,
                &[(Field::Model, &wire_model), (Field::Store, "false")],
            ),
            Upstream::OpenRouter { .. } if peek.has(Field::Models) || peek.has(Field::Route) => {
                return dialect.sandbox(
                    Problem::Invalid,
                    "OpenRouter fallback routing (`models` / `route`) is not allowed; the sandbox pins each call to one configured model. Remove the field.",
                );
            }
            Upstream::Anthropic { .. } | Upstream::OpenRouter { .. } => {
                peek.rewrite(&body, &[(Field::Model, &wire_model)])
            }
        };
        drop(body);
        let admission = if dialect == Dialect::CountTokens {
            None
        } else {
            let call = Call {
                agent: grant.agent.clone(),
                model: model.name.clone(),
                backend: model.backend,
                via: Via::Proxy,
            };
            let estimate = Estimate::from_sizes(
                Sizes {
                    bytes: rewritten.len(),
                    images: peek.images,
                },
                None,
                model.reserve,
            );
            match proxy.metering.admit(call, estimate) {
                Ok(admission) => Some(admission),
                Err(refusal) => {
                    span.record("outcome", "refused");
                    return dialect.refusal(&refusal);
                }
            }
        };
        let upstream = match send(
            &proxy.http,
            model,
            dialect,
            &headers,
            Bytes::from(rewritten),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                span.record("outcome", Outcome::Failed.as_str());
                tracing::warn!(target: "model", event = "proxy.upstream_failed", error.kind = error.kind(), error = %error);
                if let Some(admission) = admission {
                    admission.settle(match error {
                        SendError::Credential(_) | SendError::CredentialTask(_) => Outcome::NotSent,
                        SendError::Request(_) => Outcome::Failed,
                    });
                }
                return dialect.error(Problem::Upstream, "the model upstream could not be reached");
            }
        };
        respond(&proxy, dialect, upstream, admission, span).await
    }.instrument(context).await
}

#[derive(Debug, thiserror::Error)]
enum SendError {
    #[error("Codex credential is unavailable")]
    Credential(#[source] dekopon_model::chatgpt::ChatGptError),
    #[error("Codex credential task failed")]
    CredentialTask(#[source] tokio::task::JoinError),
    #[error("upstream request failed")]
    Request(#[source] reqwest::Error),
}

impl SendError {
    const fn kind(&self) -> &'static str {
        match self {
            Self::Credential(_) | Self::CredentialTask(_) => "credential",
            Self::Request(_) => "request",
        }
    }
}

async fn send(
    http: &reqwest::Client,
    model: &ProxyModel,
    dialect: Dialect,
    guest: &HeaderMap,
    body: Bytes,
) -> Result<reqwest::Response, SendError> {
    let mut request = http
        .post(model.upstream.url(dialect))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.clone());
    for name in PASSED_HEADERS {
        if let Some(value) = guest.get(name) {
            request = request.header(name, value);
        }
    }
    match &model.upstream {
        Upstream::Anthropic { api_key, .. } => request
            .header("x-api-key", api_key.expose().as_str())
            .send()
            .await
            .map_err(SendError::Request),
        Upstream::OpenRouter { api_key, .. } => request
            .bearer_auth(api_key.expose())
            .send()
            .await
            .map_err(SendError::Request),
        Upstream::Codex { credential, .. } => {
            let request = request
                .header("originator", "dekopon")
                .header("openai-beta", "responses=experimental")
                .header(header::ACCEPT, "text/event-stream");
            let retry = request.try_clone();
            let current = codex_credential(credential, None).await?;
            let response = request
                .bearer_auth(current.access.expose())
                .header("chatgpt-account-id", &current.account_id)
                .send()
                .await
                .map_err(SendError::Request)?;
            match retry {
                Some(retry) if response.status() == StatusCode::UNAUTHORIZED => {
                    drop(response);
                    let refreshed = codex_credential(credential, Some(current.access)).await?;
                    retry
                        .bearer_auth(refreshed.access.expose())
                        .header("chatgpt-account-id", &refreshed.account_id)
                        .send()
                        .await
                        .map_err(SendError::Request)
                }
                Some(_) | None => Ok(response),
            }
        }
    }
}

/// The credential file refreshes over blocking HTTP, so it runs on the blocking pool.
async fn codex_credential(
    credential: &Arc<CredentialFile>,
    rejected: Option<Redacted<String>>,
) -> Result<dekopon_model::chatgpt::ResolvedCredential, SendError> {
    let credential = Arc::clone(credential);
    tokio::task::spawn_blocking(move || match rejected {
        Some(rejected) => credential.force_refresh(&rejected),
        None => credential.current(),
    })
    .await
    .map_err(SendError::CredentialTask)?
    .map_err(SendError::Credential)
}

async fn respond(
    proxy: &ModelProxy,
    dialect: Dialect,
    upstream: reqwest::Response,
    admission: Option<dekopon_model_token_governor::Admission>,
    span: tracing::Span,
) -> Response {
    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for name in RETURNED_HEADERS {
        if let Some(value) = upstream.headers().get(name) {
            headers.insert(HeaderName::from_static(name), value.clone());
        }
    }
    if !status.is_success() {
        span.record("outcome", Outcome::NotSent.as_str());
        if let Some(admission) = admission {
            admission.settle(Outcome::NotSent);
        }
        let body = bounded(upstream, MAX_ERROR_BYTES).await;
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        return response;
    }
    let events = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let shape = if events { Shape::Events } else { Shape::Json };
    let stream = tee::tee(
        upstream.bytes_stream(),
        dialect,
        shape,
        proxy.timing,
        admission,
        span,
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    if events {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    *response.headers_mut() = headers;
    response
}

fn peek_rule(error: &PeekError, grant: &Grant) -> String {
    match error {
        PeekError::NotJson | PeekError::NotObject => {
            "the request body must be a JSON object. Send one.".to_owned()
        }
        PeekError::NoModel => format!(
            "the request must name a model. Set `model` to one of this agent's configured models: {}.",
            grant
                .models
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        PeekError::Duplicate(field) => {
            format!("the request repeats the top-level key `{field}`; send it once.")
        }
    }
}

async fn bounded(mut upstream: reqwest::Response, limit: usize) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= limit => body.extend_from_slice(&chunk),
            Ok(Some(_)) => break,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(target: "model", error = %error, "upstream error body was cut short");
                break;
            }
        }
    }
    body
}
