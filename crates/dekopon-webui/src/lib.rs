//! Unauthenticated, read-only operational web UI for `dekopon-brokerd`.
//!
//! The HTTP surface exposes only `GET`/`HEAD` views. It has no login because it has no mutation or
//! authorization path, but its contents are still deployment information: agent names, provider
//! schemas, artifact paths and digests, Wasmtime counters, and credential-free OTLP settings. An
//! operator chooses the listener address explicitly and owns the network boundary around it.
//!
//! The listener is bounded like the broker's Unix socket rather than left open-ended: see
//! [`WebUiLimits`].

#![forbid(unsafe_code)]

mod listener;
mod render;
mod status;

use std::{future::Future, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes, HttpBody as _},
    extract::{Path, Request, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use dekopon_broker_host::{BrokerHostMetrics, LoadedProviderMetadata};
use thiserror::Error;
use tokio::net::TcpListener;

use crate::listener::BoundedListener;

pub use status::{ServiceStatus, TokenTotals};

/// Concurrent connections the informational listener accepts before refusing.
pub const DEFAULT_MAX_CONNECTIONS: usize = 16;
/// Wall-clock budget one accepted connection has from accept to close.
pub const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Credential-free OTLP settings safe to render in the informational UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OtelSummary {
    /// Configured receiver base endpoint.
    pub endpoint: String,
    /// `grpc` or `http`.
    pub transport: String,
    /// Resource service name.
    pub service_name: String,
    /// Per-export timeout.
    pub export_timeout_ms: u64,
    /// Whether payload-bearing telemetry is enabled.
    pub telemetry_payloads: bool,
    /// Whether any standard OTLP header environment variable is present.
    ///
    /// Values are never retained or rendered.
    pub headers_configured: bool,
    /// Whether `OTEL_RESOURCE_ATTRIBUTES` is present. Its value is not retained or rendered.
    pub resource_attributes_configured: bool,
}

/// Ceilings the informational listener enforces on an unauthenticated network surface.
///
/// This listener lives inside the privileged broker process, whose worst deployment failure is an
/// OOM kill, so it mirrors the broker socket's `maxConnections`/`ioTimeoutMs` philosophy: a fixed
/// concurrency ceiling that **refuses** rather than queues, and one wall-clock budget per accepted
/// connection covering header read, rendering, and body write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebUiLimits {
    /// Connections served concurrently. Further connections are closed without a response.
    pub max_connections: usize,
    /// Budget one connection has from accept to close, including HTTP/1 keep-alive reuse.
    pub connection_timeout: Duration,
}

impl Default for WebUiLimits {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            connection_timeout: DEFAULT_CONNECTION_TIMEOUT,
        }
    }
}

/// One provider's startup-immutable rendered page beside the metadata it was rendered from.
#[derive(Clone, Debug)]
pub(crate) struct ProviderPage {
    metadata: LoadedProviderMetadata,
    page: Bytes,
}

impl ProviderPage {
    pub(crate) const fn metadata(&self) -> &LoadedProviderMetadata {
        &self.metadata
    }
}

/// Shared immutable deployment metadata and live process-local counters.
#[derive(Clone, Debug)]
pub struct Dashboard {
    pub(crate) version: String,
    pub(crate) providers: Arc<[ProviderPage]>,
    pub(crate) host_metrics: BrokerHostMetrics,
    pub(crate) service_status: ServiceStatus,
    pub(crate) otel: Option<OtelSummary>,
}

impl Dashboard {
    /// Builds a dashboard from provider metadata captured during broker startup.
    ///
    /// Provider pages derive solely from that metadata and the broker version, both fixed for the
    /// life of the process, so each page is serialized, escaped, and rendered exactly once here
    /// instead of on every request.
    #[must_use]
    pub fn new(
        version: impl Into<String>,
        mut providers: Vec<LoadedProviderMetadata>,
        host_metrics: BrokerHostMetrics,
        service_status: ServiceStatus,
        otel: Option<OtelSummary>,
    ) -> Self {
        let version = version.into();
        providers.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
        let providers = providers
            .into_iter()
            .map(|metadata| ProviderPage {
                page: Bytes::from(render::provider_page(&version, &metadata)),
                metadata,
            })
            .collect();
        Self {
            version,
            providers,
            host_metrics,
            service_status,
            otel,
        }
    }

    fn provider(&self, id: &str) -> Option<&ProviderPage> {
        self.providers
            .iter()
            .find(|provider| provider.metadata.manifest.id.as_str() == id)
    }
}

/// Builds the GET-only router used by `dekopon-brokerd`.
pub fn router(dashboard: Dashboard) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/ui", get(index))
        .route("/ui/", get(ui_slash))
        .route("/ui/providers/{provider}", get(provider))
        .fallback(not_found)
        // Applied to the whole router rather than inside the response helpers so the closed policy
        // also covers responses this crate never authors, notably axum's 405 for a mutating method.
        .layer(axum::middleware::map_response(apply_secure_headers))
        .layer(axum::middleware::from_fn(trace_request))
        .with_state(dashboard)
}

/// Serves the dashboard on an already-bound listener until graceful shutdown.
///
/// Connections are bounded by [`WebUiLimits::default`].
pub async fn serve<F>(
    listener: TcpListener,
    dashboard: Dashboard,
    shutdown: F,
) -> Result<(), WebUiError>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_with_limits(listener, dashboard, WebUiLimits::default(), shutdown).await
}

/// Serves the dashboard with explicit connection ceilings.
pub async fn serve_with_limits<F>(
    listener: TcpListener,
    dashboard: Dashboard,
    limits: WebUiLimits,
    shutdown: F,
) -> Result<(), WebUiError>
where
    F: Future<Output = ()> + Send + 'static,
{
    if limits.max_connections == 0 || limits.connection_timeout.is_zero() {
        return Err(WebUiError::InvalidLimits);
    }
    axum::serve(BoundedListener::new(listener, limits), router(dashboard))
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(WebUiError::Serve)
}

async fn root() -> Response {
    redirect("/ui")
}

async fn ui_slash() -> Response {
    redirect("/ui")
}

async fn index(State(dashboard): State<Dashboard>) -> Response {
    html(StatusCode::OK, render::dashboard(&dashboard))
}

async fn provider(State(dashboard): State<Dashboard>, Path(provider): Path<String>) -> Response {
    match dashboard.provider(&provider) {
        Some(provider) => rendered(StatusCode::OK, provider.page.clone()),
        None => html(
            StatusCode::NOT_FOUND,
            render::not_found(&dashboard, "Provider not found"),
        ),
    }
}

async fn not_found(State(dashboard): State<Dashboard>) -> Response {
    html(
        StatusCode::NOT_FOUND,
        render::not_found(&dashboard, "Page not found"),
    )
}

fn redirect(location: &'static str) -> Response {
    Redirect::permanent(location).into_response()
}

fn html(status: StatusCode, body: String) -> Response {
    (status, Html(body)).into_response()
}

fn rendered(status: StatusCode, body: Bytes) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        Body::from(body),
    )
        .into_response()
}

async fn apply_secure_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
        ),
    );
    response
}

/// Records one line per request so probe traffic and path scans on this listener are observable.
///
/// Debug level: an `info` production filter ships nothing, and the signal is one `RUST_LOG` away.
/// The path is recorded without its query string, and no request or response body ever is.
async fn trace_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    tracing::debug!(
        event = "webui_request",
        http.method = %method,
        http.path = %path,
        http.status = response.status().as_u16(),
        http.response_bytes = response.body().size_hint().exact().unwrap_or_default()
    );
    response
}

/// HTTP listener or serving failure.
#[derive(Debug, Error)]
pub enum WebUiError {
    /// Axum could not serve or gracefully close the listener.
    #[error("Dekopon web UI server failed")]
    Serve(#[source] std::io::Error),
    /// Connection ceilings would have left the listener unbounded.
    #[error("Dekopon web UI connection limits are invalid")]
    InvalidLimits,
}
