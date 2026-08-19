//! Unauthenticated, read-only operational web UI for `dekopon-brokerd`.
//!
//! The HTTP surface exposes only `GET`/`HEAD` views. It has no login because it has no mutation or
//! authorization path, but its contents are still deployment information: agent names, provider
//! schemas, artifact paths and digests, Wasmtime counters, and credential-free OTLP settings. An
//! operator chooses the listener address explicitly and owns the network boundary around it.

#![forbid(unsafe_code)]

mod render;
mod status;

use std::{future::Future, sync::Arc};

use axum::{
    Router,
    extract::{Path, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use dekopon_broker_host::{BrokerHostMetrics, LoadedProviderMetadata};
use thiserror::Error;
use tokio::net::TcpListener;

pub use status::{ServiceStatus, TokenTotals};

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

/// Shared immutable deployment metadata and live process-local counters.
#[derive(Clone, Debug)]
pub struct Dashboard {
    pub(crate) version: String,
    pub(crate) providers: Arc<[LoadedProviderMetadata]>,
    pub(crate) host_metrics: BrokerHostMetrics,
    pub(crate) service_status: ServiceStatus,
    pub(crate) otel: Option<OtelSummary>,
}

impl Dashboard {
    /// Builds a dashboard from provider metadata captured during broker startup.
    #[must_use]
    pub fn new(
        version: impl Into<String>,
        mut providers: Vec<LoadedProviderMetadata>,
        host_metrics: BrokerHostMetrics,
        service_status: ServiceStatus,
        otel: Option<OtelSummary>,
    ) -> Self {
        providers.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));
        Self {
            version: version.into(),
            providers: providers.into(),
            host_metrics,
            service_status,
            otel,
        }
    }

    fn provider(&self, id: &str) -> Option<&LoadedProviderMetadata> {
        self.providers
            .iter()
            .find(|provider| provider.manifest.id.as_str() == id)
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
        .with_state(dashboard)
}

/// Serves the dashboard on an already-bound listener until graceful shutdown.
pub async fn serve<F>(
    listener: TcpListener,
    dashboard: Dashboard,
    shutdown: F,
) -> Result<(), WebUiError>
where
    F: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(dashboard))
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
        Some(provider) => html(StatusCode::OK, render::provider(&dashboard, provider)),
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
    let mut response = Redirect::permanent(location).into_response();
    secure_headers(response.headers_mut());
    response
}

fn html(status: StatusCode, body: String) -> Response {
    let mut response = (status, Html(body)).into_response();
    secure_headers(response.headers_mut());
    response
}

fn secure_headers(headers: &mut axum::http::HeaderMap) {
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
}

/// HTTP listener or serving failure.
#[derive(Debug, Error)]
pub enum WebUiError {
    /// Axum could not serve or gracefully close the listener.
    #[error("Dekopon web UI server failed")]
    Serve(#[source] std::io::Error),
}

#[cfg(test)]
mod tests;
