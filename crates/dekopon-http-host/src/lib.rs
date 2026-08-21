//! Statically linked, buffered, deny-by-default HTTP primitive for Dekopon's broker host.
//!
//! This crate performs no authorization transition. It consumes a broker-produced
//! [`HttpConstraints`] value beneath independent native ceilings and returns bounded buffers plus
//! sanitized evidence metadata. Provider-facing WIT conversion remains in `dekopon-broker-host`.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeSet,
    error::Error as _,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use dekopon_capability::{HttpConstraints, HttpConstraintsError};
use dekopon_core::Redacted;
use futures_util::StreamExt as _;
use reqwest::{
    Method, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
    redirect,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::{Instant, timeout};

const DEFAULT_HTTPS_PORT: u16 = 443;
const MAX_ERROR_MESSAGE_BYTES: usize = 256;
const MAX_RESOLVED_ADDRESSES: usize = 16;
const REQUEST_ENCODING_OVERHEAD_BYTES: u64 = 128;

/// Default maximum calls accepted by one native HTTP execution context.
pub const DEFAULT_MAX_REQUESTS: u32 = 32;
/// Default maximum accounted request bytes (1 MiB).
pub const DEFAULT_MAX_REQUEST_BYTES: u64 = 1024 * 1024;
/// Default maximum accounted response bytes (4 MiB).
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
/// Default maximum header values in either direction.
pub const DEFAULT_MAX_HEADERS: usize = 128;
/// Default maximum aggregate header bytes in either direction (64 KiB).
pub const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1024;
/// Maximum secret bytes accepted into one bound credential.
pub const MAX_CREDENTIAL_BYTES: usize = 4096;

/// One ordered byte-valued HTTP header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    /// Case-insensitive HTTP field name.
    pub name: String,
    /// Field value bytes.
    pub value: Vec<u8>,
}

/// One complete buffered HTTP request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// Any syntactically valid standard or extension method token.
    pub method: String,
    /// Absolute HTTP or HTTPS URI.
    pub uri: String,
    /// Ordered headers with duplicate names preserved.
    pub headers: Vec<Header>,
    /// Complete body bytes.
    pub body: Vec<u8>,
}

/// One complete buffered HTTP response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Ordered non-sensitive end-to-end headers.
    pub headers: Vec<Header>,
    /// Complete body bytes.
    pub body: Vec<u8>,
}

/// Stable failure classes mapped to the component contract by the broker host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    /// Invalid method token.
    InvalidMethod,
    /// Invalid or unsupported URI.
    InvalidUri,
    /// Invalid or broker-owned header.
    InvalidHeader,
    /// Request exceeded a byte or header bound.
    RequestTooLarge,
    /// Destination, scheme, or method was denied.
    Denied,
    /// Invocation exhausted its HTTP call count.
    HostCallLimit,
    /// DNS resolution failed.
    Dns,
    /// Connection failed.
    Connect,
    /// TLS validation or negotiation failed.
    Tls,
    /// Deadline expired.
    Timeout,
    /// HTTP protocol failed.
    Protocol,
    /// Response exceeded a byte or header bound.
    ResponseTooLarge,
    /// Native client setup failed.
    Internal,
}

/// Bounded provider-safe HTTP failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpError {
    /// Stable machine class.
    pub code: ErrorCode,
    /// Sanitized detail of at most 256 UTF-8 bytes.
    pub message: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for HttpError {}

/// Independent native ceilings that authorization cannot widen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpHostCeilings {
    /// Maximum calls in one execution context.
    pub max_requests: u32,
    /// Maximum authorized accounted request bytes.
    pub max_request_bytes: u64,
    /// Maximum authorized accounted response bytes.
    pub max_response_bytes: u64,
    /// Maximum header values in a request or response.
    pub max_headers: usize,
    /// Maximum aggregate header bytes in a request or response.
    pub max_header_bytes: usize,
}

impl Default for HttpHostCeilings {
    fn default() -> Self {
        Self {
            max_requests: DEFAULT_MAX_REQUESTS,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_headers: DEFAULT_MAX_HEADERS,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
        }
    }
}

/// Invalid native ceiling or broker grant.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ConfigurationError {
    /// A native ceiling was zero.
    #[error("HTTP host ceiling {field} must be greater than zero")]
    ZeroCeiling {
        /// Invalid field.
        field: &'static str,
    },
    /// Grant omitted required authority, an exact entry, or a positive bound.
    #[error("HTTP authorization is not exact: {source}")]
    InvalidGrant {
        /// Which entry-grammar or bound rule the grant violated.
        #[source]
        source: HttpConstraintsError,
    },
    /// Grant attempted to exceed a native ceiling.
    #[error("HTTP authorization exceeds native host ceilings")]
    GrantExceedsCeiling,
    /// Execution deadline was zero.
    #[error("HTTP execution deadline must be greater than zero")]
    ZeroTimeout,
    /// Execution deadline could not be represented by the runtime clock.
    #[error("HTTP execution deadline is too large")]
    TimeoutOverflow,
    /// A bound credential failed structural validation.
    ///
    /// The reason names the invalid *field*, never any part of the secret value.
    #[error("bound credential is invalid: {reason}")]
    InvalidCredential {
        /// Which structural rule the credential violated.
        reason: &'static str,
    },
}

/// A broker-resolved secret pre-rendered as one `authorization` header value and bound to
/// explicit destination authorities.
///
/// This is the carrier for "an agent may *use* a credential but never see it": the broker
/// constructs one from owner-only storage, hands it to the execution context alongside the
/// authorization grant, and [`BufferedHttpClient`] injects it after guest headers have been
/// validated — a guest-supplied `authorization` header is still rejected, never overwritten. The
/// value lives in a [`Redacted`] wrapper end to end, so no `Debug`, `Display`, or `Serialize`
/// path renders it.
///
/// Destinations use the same authority grammar as `HttpConstraints::allowed_hosts` (`host` for
/// HTTPS on 443, `host:port` otherwise) and are matched with the same rules, so a policy layer
/// that requires every allowed host to appear verbatim in the credential's destinations makes a
/// runtime mismatch unreachable by construction. The request-time check here remains as defense
/// in depth and fails closed: a credentialed context never sends an unauthenticated request to a
/// destination outside the binding.
#[derive(Clone)]
pub struct BoundCredential {
    header_value: Redacted<String>,
    destinations: Vec<String>,
}

impl fmt::Debug for BoundCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundCredential")
            .field("header_value", &self.header_value)
            .field("destinations", &self.destinations)
            .finish()
    }
}

impl BoundCredential {
    /// Builds a scheme-prefixed credential such as `Bearer <secret>` or `token <secret>`.
    pub fn bearer(
        scheme: &str,
        secret: Redacted<String>,
        destinations: Vec<String>,
    ) -> Result<Self, ConfigurationError> {
        let invalid = |reason| ConfigurationError::InvalidCredential { reason };
        if scheme.is_empty() || !scheme.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(invalid("scheme must be a printable ASCII token"));
        }
        {
            let value = secret.expose();
            if value.is_empty() {
                return Err(invalid("secret must not be empty"));
            }
            if value.len() > MAX_CREDENTIAL_BYTES {
                return Err(invalid("secret exceeds the credential byte limit"));
            }
            if !value
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
            {
                return Err(invalid("secret contains bytes invalid in a header value"));
            }
        }
        if destinations.is_empty() {
            return Err(invalid("at least one destination is required"));
        }
        if !destinations
            .iter()
            .all(|destination| is_destination_scope(destination))
        {
            return Err(invalid(
                "destinations must be host or host:port authorities",
            ));
        }
        let header_value = Redacted::new(format!("{scheme} {}", secret.expose()));
        Ok(Self {
            header_value,
            destinations,
        })
    }

    /// The authorities this credential may be presented to, for policy-layer coverage checks.
    #[must_use]
    pub fn destinations(&self) -> &[String] {
        &self.destinations
    }

    /// Whether one allowed-host scope is covered verbatim by this credential's destinations.
    #[must_use]
    pub fn covers(&self, allowed_host: &str) -> bool {
        let allowed = allowed_host.trim().to_ascii_lowercase();
        self.destinations
            .iter()
            .any(|destination| destination.trim().to_ascii_lowercase() == allowed)
    }

    fn matches(&self, host: &str, port: u16, scheme: &str) -> bool {
        self.destinations
            .iter()
            .any(|destination| authority_matches(destination, host, port, scheme))
    }

    /// Renders the header value, marked sensitive so the transport never debugs it.
    fn render(&self) -> Result<HeaderValue, HttpError> {
        let mut value = HeaderValue::from_str(self.header_value.expose()).map_err(|_| {
            // Unreachable when construction validated the bytes; message carries no value detail.
            http_error(ErrorCode::Internal, "credential could not be rendered")
        })?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// The `allowed_hosts` grammar, applied to credential destinations: a bare authority with no
/// path, query, fragment, userinfo, wildcard, or whitespace.
fn is_destination_scope(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        && !value.contains(['/', '?', '#', '@', '*'])
}

/// Sanitized metadata for one attempted native request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpCallEvidence {
    /// HTTP method selected by the provider.
    pub method: String,
    /// Authorized destination authority. Paths and queries are intentionally omitted.
    pub authority: String,
    /// Response status when a response was received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Conservative request bytes accounted by the host.
    ///
    /// Counts guest-authored content only. A broker-injected credential header is deliberately
    /// excluded: its length would leak into public evidence, and guest byte grants must not need
    /// padding for a secret the guest cannot see.
    pub request_bytes: u64,
    /// Conservative response bytes accounted by the host.
    pub response_bytes: u64,
    /// Whether the broker injected a destination-bound credential into this request.
    ///
    /// Presence is audit-relevant — the call transacted with broker-held authority — while the
    /// value appears nowhere. Absent from records written before credentials existed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub credential_injected: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Per-invocation buffered HTTP execution context.
#[derive(Debug)]
pub struct BufferedHttpClient {
    grant: Option<HttpConstraints>,
    credential: Option<BoundCredential>,
    ceilings: HttpHostCeilings,
    deadline: Instant,
    calls: u32,
    attempted: bool,
    policy_violation: Option<&'static str>,
    evidence: Vec<HttpCallEvidence>,
}

impl BufferedHttpClient {
    /// Creates a disabled context used while describing providers or for invocations without HTTP.
    pub fn disabled(
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        validate_configuration(None, &ceilings, timeout)?;
        Self::new(None, None, ceilings, timeout)
    }

    /// Creates a context constrained by one broker-produced HTTP grant.
    pub fn authorized(
        grant: HttpConstraints,
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        validate_configuration(Some(&grant), &ceilings, timeout)?;
        Self::new(Some(grant), None, ceilings, timeout)
    }

    /// Creates an authorized context that additionally presents one destination-bound credential.
    ///
    /// The credential is injected after every guest-facing check has run, only for requests whose
    /// resolved authority falls inside the credential's destination binding; a request outside it
    /// is refused rather than sent unauthenticated.
    pub fn authorized_with_credential(
        grant: HttpConstraints,
        credential: Option<BoundCredential>,
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        validate_configuration(Some(&grant), &ceilings, timeout)?;
        Self::new(Some(grant), credential, ceilings, timeout)
    }

    fn new(
        grant: Option<HttpConstraints>,
        credential: Option<BoundCredential>,
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(ConfigurationError::TimeoutOverflow)?;
        Ok(Self {
            grant,
            credential,
            ceilings,
            deadline,
            calls: 0,
            attempted: false,
            policy_violation: None,
            evidence: Vec::new(),
        })
    }

    /// Whether provider code attempted any HTTP call.
    pub fn attempted(&self) -> bool {
        self.attempted
    }

    /// Stable enforcement reason that provider code must not mask.
    pub fn policy_violation(&self) -> Option<&'static str> {
        self.policy_violation
    }

    /// Consumes the context and returns sanitized call metadata.
    pub fn into_evidence(self) -> Vec<HttpCallEvidence> {
        self.evidence
    }

    /// Executes one request beneath both the broker grant and native ceilings.
    pub async fn send(&mut self, request: Request) -> Result<Response, HttpError> {
        // Fields mirror `HttpCallEvidence` exactly, and that is the point rather than a
        // coincidence: this span is a second egress path for the same call the audit chain
        // records, so it carries the same sanitized set and no more. URL paths and queries,
        // request and response headers, and both bodies are absent here for the same reason they
        // are absent from evidence — a trace backend is not an audit boundary.
        let span = tracing::info_span!(
            "http.request",
            "http.request.method" = tracing::field::Empty,
            "server.address" = tracing::field::Empty,
            "http.response.status_code" = tracing::field::Empty,
            "http.request.body.size" = tracing::field::Empty,
            "http.response.body.size" = tracing::field::Empty,
            "url.full" = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        // Opt-in only. `url.full` is the field that carries a path and query, which is exactly
        // what the metadata-only default withholds.
        if dekopon_core::telemetry_payloads() {
            span.record("url.full", request.uri.as_str());
        }
        let _entered = span.enter();

        let evidence_index = self.evidence.len();
        self.attempted = true;
        let result = self.send_checked(request).await;

        // `prepare` is what learns the method and authority, so the evidence entry it pushed is
        // the only sanitized source for them; a request rejected before that point simply has
        // nothing safe to report beyond its outcome.
        if let Some(evidence) = self.evidence.get(evidence_index) {
            span.record("http.request.method", evidence.method.as_str());
            span.record("server.address", evidence.authority.as_str());
            span.record("http.request.body.size", evidence.request_bytes);
            span.record("http.response.body.size", evidence.response_bytes);
            if let Some(status) = evidence.status {
                span.record("http.response.status_code", status);
            }
        }
        span.record("outcome", outcome_label(&result));

        // Accounting rather than lifecycle: the `http.request` span above already carries these
        // fields and its own duration. This record exists to outlive trace retention and survive
        // sampling, because an external request is a billed and rate-limited call, and "how many,
        // to where, how big" is asked long after the trace has expired. Same sanitized set — no
        // path, query, header, or body — for the same reason.
        if let Some(evidence) = self.evidence.get(evidence_index) {
            tracing::info!(
                target: "dekopon_http_host::audit",
                {
                    audit.event = "accounting.http.request",
                    "http.request.method" = evidence.method.as_str(),
                    "server.address" = evidence.authority.as_str(),
                    "http.response.status_code" = evidence.status.unwrap_or_default(),
                    "http.request.body.size" = evidence.request_bytes,
                    "http.response.body.size" = evidence.response_bytes,
                    outcome = outcome_label(&result),
                },
                "external HTTP request accounted"
            );
        }

        if let Err(error) = &result {
            self.policy_violation = match &error.code {
                ErrorCode::Denied => Some("denied"),
                ErrorCode::HostCallLimit => Some("host-call-limit"),
                ErrorCode::InvalidMethod | ErrorCode::InvalidUri | ErrorCode::InvalidHeader => {
                    Some("invalid-http-request")
                }
                ErrorCode::RequestTooLarge | ErrorCode::ResponseTooLarge => Some("byte-limit"),
                _ => self.policy_violation,
            };
        }
        result
    }

    async fn send_checked(&mut self, request: Request) -> Result<Response, HttpError> {
        let Some(grant) = self.grant.clone() else {
            return Err(http_error(
                ErrorCode::Denied,
                "this invocation has no HTTP authorization",
            ));
        };
        if self.calls >= grant.max_requests {
            return Err(http_error(
                ErrorCode::HostCallLimit,
                "the authorized HTTP request limit is exhausted",
            ));
        }
        self.calls = self.calls.saturating_add(1);

        let mut prepared = self.prepare(request, &grant).await?;
        // Injection happens strictly after `prepare`, so every guest-facing rule has already run:
        // a guest-supplied `authorization` header was rejected (never overwritten), and the
        // destination passed the grant. The credential's own binding is narrower than the grant's
        // and fails closed — a credentialed context refuses to send an unauthenticated request to
        // an allowed-but-unbound destination, because "quietly missing auth" reads as an outage
        // at best and an unauthenticated write at worst.
        let mut credential_injected = false;
        if let Some(credential) = &self.credential {
            let port = prepared
                .url
                .port_or_known_default()
                .unwrap_or(DEFAULT_HTTPS_PORT);
            if !credential.matches(&prepared.host, port, prepared.url.scheme()) {
                return Err(http_error(
                    ErrorCode::Denied,
                    "destination is outside this credential's binding",
                ));
            }
            prepared.headers.insert(AUTHORIZATION, credential.render()?);
            credential_injected = true;
        }
        let evidence_index = self.evidence.len();
        self.evidence.push(HttpCallEvidence {
            method: prepared.method.as_str().to_owned(),
            authority: prepared.authority.clone(),
            status: None,
            request_bytes: prepared.request_bytes,
            response_bytes: 0,
            credential_injected,
        });

        let result = self.execute(prepared, &grant).await;
        if let Ok((response, response_bytes)) = &result {
            let evidence = &mut self.evidence[evidence_index];
            evidence.status = Some(response.status);
            evidence.response_bytes = *response_bytes;
        }
        result.map(|(response, _bytes)| response)
    }

    async fn prepare(
        &self,
        request: Request,
        grant: &HttpConstraints,
    ) -> Result<PreparedRequest, HttpError> {
        let method = Method::from_bytes(request.method.as_bytes()).map_err(|_| {
            http_error(ErrorCode::InvalidMethod, "method is not a valid HTTP token")
        })?;
        if !grant
            .allowed_methods
            .iter()
            .any(|allowed| allowed == method.as_str())
        {
            return Err(http_error(
                ErrorCode::Denied,
                "HTTP method is not authorized for this invocation",
            ));
        }
        let minimum_request_bytes =
            encoded_request_bytes(method.as_str(), &request.uri, 0, request.body.len() as u64)
                .ok_or_else(|| http_error(ErrorCode::RequestTooLarge, "request size overflowed"))?;
        if minimum_request_bytes > grant.max_request_bytes {
            return Err(http_error(
                ErrorCode::RequestTooLarge,
                "request exceeds the authorized byte limit",
            ));
        }

        let url = Url::parse(&request.uri)
            .map_err(|_| http_error(ErrorCode::InvalidUri, "URI is not a valid absolute URL"))?;
        if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
            return Err(http_error(
                ErrorCode::InvalidUri,
                "URI user information and fragments are prohibited",
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| http_error(ErrorCode::InvalidUri, "URI has no host"))?
            .to_ascii_lowercase();
        let port = url.port_or_known_default().ok_or_else(|| {
            http_error(
                ErrorCode::InvalidUri,
                "URI scheme has no recognized default port",
            )
        })?;
        let authority = canonical_authority(&host, port);
        if !grant
            .allowed_hosts
            .iter()
            .any(|allowed| authority_matches(allowed, &host, port, url.scheme()))
        {
            return Err(http_error(
                ErrorCode::Denied,
                "HTTP destination is not authorized for this invocation",
            ));
        }

        match url.scheme() {
            "https" => {}
            "http" if grant.allow_plaintext_loopback => {}
            "http" => {
                return Err(http_error(
                    ErrorCode::Denied,
                    "plaintext HTTP is not authorized",
                ));
            }
            _ => {
                return Err(http_error(
                    ErrorCode::InvalidUri,
                    "only HTTP and HTTPS URLs are supported",
                ));
            }
        }

        if request.headers.len() > self.ceilings.max_headers {
            return Err(http_error(
                ErrorCode::RequestTooLarge,
                "request has too many headers",
            ));
        }
        let mut headers = HeaderMap::new();
        let mut header_bytes = 0_u64;
        for header in request.headers {
            header_bytes = header_bytes
                .checked_add(header.name.len() as u64)
                .and_then(|size| size.checked_add(header.value.len() as u64))
                .and_then(|size| size.checked_add(4))
                .ok_or_else(|| {
                    http_error(ErrorCode::RequestTooLarge, "request header size overflowed")
                })?;
            if header_bytes > self.ceilings.max_header_bytes as u64 {
                return Err(http_error(
                    ErrorCode::RequestTooLarge,
                    "request headers exceed the host limit",
                ));
            }
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| {
                http_error(ErrorCode::InvalidHeader, "request header name is invalid")
            })?;
            if is_forbidden_request_header(&name) {
                return Err(http_error(
                    ErrorCode::InvalidHeader,
                    "request header is broker-owned or hop-by-hop",
                ));
            }
            let value = HeaderValue::from_bytes(&header.value).map_err(|_| {
                http_error(ErrorCode::InvalidHeader, "request header value is invalid")
            })?;
            headers.append(name, value);
        }

        let request_bytes = encoded_request_bytes(
            method.as_str(),
            url.as_str(),
            header_bytes,
            request.body.len() as u64,
        )
        .ok_or_else(|| http_error(ErrorCode::RequestTooLarge, "request size overflowed"))?;
        if request_bytes > grant.max_request_bytes {
            return Err(http_error(
                ErrorCode::RequestTooLarge,
                "request exceeds the authorized byte limit",
            ));
        }

        let remaining = self.remaining()?;
        let addresses = timeout(remaining, resolve_destination(&host, port))
            .await
            .map_err(|_| http_error(ErrorCode::Timeout, "destination resolution timed out"))??;
        let all_loopback = addresses.iter().all(|address| address.ip().is_loopback());
        if url.scheme() == "http" && !all_loopback {
            return Err(http_error(
                ErrorCode::Denied,
                "plaintext HTTP is restricted to loopback destinations",
            ));
        }
        if url.scheme() == "https"
            && addresses
                .iter()
                .any(|address| is_forbidden_public_destination(address.ip()))
        {
            return Err(http_error(
                ErrorCode::Denied,
                "destination resolved to a non-public address",
            ));
        }

        Ok(PreparedRequest {
            method,
            url,
            headers,
            body: request.body,
            host,
            authority,
            addresses,
            request_bytes,
        })
    }

    async fn execute(
        &self,
        request: PreparedRequest,
        grant: &HttpConstraints,
    ) -> Result<(Response, u64), HttpError> {
        let remaining = self.remaining()?;
        let client = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .no_proxy()
            .connect_timeout(remaining)
            .timeout(remaining)
            .resolve_to_addrs(&request.host, &request.addresses)
            .build()
            .map_err(|error| map_reqwest_error(&error))?;

        let response = timeout(
            remaining,
            client
                .request(request.method, request.url)
                .headers(request.headers)
                .body(request.body)
                .send(),
        )
        .await
        .map_err(|_| http_error(ErrorCode::Timeout, "HTTP request timed out"))?
        .map_err(|error| map_reqwest_error(&error))?;

        let status = response.status().as_u16();
        let mut headers = Vec::with_capacity(response.headers().len());
        let mut response_bytes = 16_u64;
        if response.headers().len() > self.ceilings.max_headers {
            return Err(http_error(
                ErrorCode::ResponseTooLarge,
                "response has too many headers",
            ));
        }
        for (name, value) in response.headers() {
            response_bytes = response_bytes
                .checked_add(name.as_str().len() as u64)
                .and_then(|size| size.checked_add(value.as_bytes().len() as u64))
                .and_then(|size| size.checked_add(4))
                .ok_or_else(|| {
                    http_error(ErrorCode::ResponseTooLarge, "response size overflowed")
                })?;
            if response_bytes > self.ceilings.max_header_bytes as u64
                || response_bytes > grant.max_response_bytes
            {
                return Err(http_error(
                    ErrorCode::ResponseTooLarge,
                    "response headers exceed an authorized bound",
                ));
            }
            if !is_forbidden_response_header(name) {
                headers.push(Header {
                    name: name.as_str().to_owned(),
                    value: value.as_bytes().to_vec(),
                });
            }
        }

        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        loop {
            let remaining = self.remaining()?;
            let chunk = match timeout(remaining, stream.next())
                .await
                .map_err(|_| http_error(ErrorCode::Timeout, "response body timed out"))?
            {
                Some(chunk) => chunk.map_err(|error| map_reqwest_error(&error))?,
                None => break,
            };
            response_bytes = response_bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| {
                    http_error(ErrorCode::ResponseTooLarge, "response size overflowed")
                })?;
            if response_bytes > grant.max_response_bytes {
                return Err(http_error(
                    ErrorCode::ResponseTooLarge,
                    "response exceeds the authorized byte limit",
                ));
            }
            body.extend_from_slice(&chunk);
        }

        Ok((
            Response {
                status,
                headers,
                body,
            },
            response_bytes,
        ))
    }

    fn remaining(&self) -> Result<Duration, HttpError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| http_error(ErrorCode::Timeout, "invocation deadline expired"))
    }
}

fn validate_configuration(
    grant: Option<&HttpConstraints>,
    ceilings: &HttpHostCeilings,
    timeout: Duration,
) -> Result<(), ConfigurationError> {
    for (field, value) in [
        ("max_requests", u128::from(ceilings.max_requests)),
        ("max_request_bytes", u128::from(ceilings.max_request_bytes)),
        (
            "max_response_bytes",
            u128::from(ceilings.max_response_bytes),
        ),
        ("max_headers", ceilings.max_headers as u128),
        ("max_header_bytes", ceilings.max_header_bytes as u128),
    ] {
        if value == 0 {
            return Err(ConfigurationError::ZeroCeiling { field });
        }
    }
    if timeout.is_zero() {
        return Err(ConfigurationError::ZeroTimeout);
    }
    if let Some(grant) = grant {
        // One shared definition of an exact grant, so this host cannot accept a scope the
        // broker that issued it would have refused.
        grant
            .validate()
            .map_err(|source| ConfigurationError::InvalidGrant { source })?;
        if grant.max_requests > ceilings.max_requests
            || grant.max_request_bytes > ceilings.max_request_bytes
            || grant.max_response_bytes > ceilings.max_response_bytes
        {
            return Err(ConfigurationError::GrantExceedsCeiling);
        }
    }
    Ok(())
}

#[derive(Debug)]
struct PreparedRequest {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Vec<u8>,
    host: String,
    authority: String,
    addresses: Vec<SocketAddr>,
    request_bytes: u64,
}

async fn resolve_destination(host: &str, port: u16) -> Result<Vec<SocketAddr>, HttpError> {
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| http_error(ErrorCode::Dns, "destination could not be resolved"))?;
    bounded_addresses(addresses)
}

fn bounded_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, HttpError> {
    let mut unique = BTreeSet::new();
    for (index, address) in addresses.into_iter().enumerate() {
        if index >= MAX_RESOLVED_ADDRESSES {
            return Err(http_error(
                ErrorCode::Dns,
                "destination resolved to too many addresses",
            ));
        }
        unique.insert(address);
    }
    if unique.is_empty() {
        return Err(http_error(
            ErrorCode::Dns,
            "destination resolved to no addresses",
        ));
    }
    Ok(unique.into_iter().collect())
}

fn canonical_authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn authority_matches(allowed: &str, host: &str, port: u16, scheme: &str) -> bool {
    let allowed = allowed.trim().to_ascii_lowercase();
    allowed == canonical_authority(host, port)
        || (scheme == "https" && allowed == host && port == DEFAULT_HTTPS_PORT)
}

fn is_forbidden_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "connection"
            | "content-length"
            | "cookie"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn is_forbidden_response_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "www-authenticate"
    )
}

fn encoded_request_bytes(
    method: &str,
    uri: &str,
    header_bytes: u64,
    body_bytes: u64,
) -> Option<u64> {
    REQUEST_ENCODING_OVERHEAD_BYTES
        .checked_add(method.len() as u64)?
        .checked_add(uri.len() as u64)?
        .checked_add(header_bytes)?
        .checked_add(body_bytes)
}

fn is_forbidden_public_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_forbidden_ipv4(ip),
        IpAddr::V6(ip) => is_forbidden_ipv6(ip),
    }
}

fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 88 && c == 99)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224
}

fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_forbidden_ipv4(ipv4);
    }
    let segments = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        // Permit only global-unicast 2000::/3, then remove IETF special-purpose 2001::/23.
        || (segments[0] & 0xe000) != 0x2000
        || (segments[0] == 0x2001 && (segments[1] & 0xfe00) == 0)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // Exclude transition/documentation ranges that are not direct public destinations.
        || segments[0] == 0x2002
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
}

fn map_reqwest_error(error: &reqwest::Error) -> HttpError {
    let code = if error.is_timeout() {
        ErrorCode::Timeout
    } else if error.is_connect() {
        if error_chain_contains(error, &["tls", "certificate", "cert "]) {
            ErrorCode::Tls
        } else {
            ErrorCode::Connect
        }
    } else {
        ErrorCode::Protocol
    };
    http_error(code, "HTTP transport failed")
}

fn error_chain_contains(error: &reqwest::Error, needles: &[&str]) -> bool {
    let mut source = error.source();
    while let Some(current) = source {
        let message = current.to_string().to_ascii_lowercase();
        if needles.iter().any(|needle| message.contains(needle)) {
            return true;
        }
        source = current.source();
    }
    false
}

pub(crate) fn http_error(code: ErrorCode, message: impl AsRef<str>) -> HttpError {
    HttpError {
        code,
        message: bounded_message(message.as_ref()),
    }
}

fn bounded_message(message: &str) -> String {
    let mut output = String::with_capacity(message.len().min(MAX_ERROR_MESSAGE_BYTES));
    for character in message.chars() {
        if output.len() + character.len_utf8() > MAX_ERROR_MESSAGE_BYTES {
            break;
        }
        if character.is_control() {
            output.push(' ');
        } else {
            output.push(character);
        }
    }
    output
}

/// One outcome vocabulary for the span and the accounting record, so they cannot disagree.
fn outcome_label(result: &Result<Response, HttpError>) -> &'static str {
    match result {
        Ok(_) => "succeeded",
        Err(error) => match &error.code {
            ErrorCode::Denied => "denied",
            ErrorCode::HostCallLimit => "host-call-limit",
            ErrorCode::InvalidMethod | ErrorCode::InvalidUri | ErrorCode::InvalidHeader => {
                "invalid-http-request"
            }
            ErrorCode::RequestTooLarge | ErrorCode::ResponseTooLarge => "byte-limit",
            _ => "failed",
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener},
        sync::mpsc::{self, Receiver},
        thread,
        time::Duration,
    };

    use dekopon_capability::{HttpConstraints, HttpConstraintsError};
    use dekopon_core::Redacted;

    use super::{
        BoundCredential, BufferedHttpClient, ConfigurationError, ErrorCode, Header,
        HttpCallEvidence, HttpHostCeilings, Request, authority_matches, bounded_addresses,
        bounded_message, is_forbidden_public_destination,
    };

    fn grant(authority: String, method: &str) -> HttpConstraints {
        HttpConstraints {
            allowed_hosts: vec![authority],
            allowed_methods: vec![method.to_owned()],
            max_requests: 2,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
        }
    }

    fn mock_http(response: &[u8]) -> (String, Receiver<Vec<u8>>, thread::JoinHandle<()>) {
        let response = response.to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        let address = listener.local_addr().expect("fixture address");
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set fixture timeout");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let mut expected = None;
            loop {
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let complete = header_end + 4 + content_length(&request[..header_end + 4]);
                    expected = Some(complete);
                    if request.len() >= complete {
                        break;
                    }
                }
                let read = stream.read(&mut buffer).expect("read fixture request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            if let Some(expected) = expected {
                request.truncate(expected);
            }
            sender.send(request).expect("record fixture request");
            stream.write_all(&response).expect("write fixture response");
            stream.flush().expect("flush fixture response");
        });
        (format!("127.0.0.1:{}", address.port()), receiver, handle)
    }

    fn content_length(headers: &[u8]) -> usize {
        String::from_utf8_lossy(headers)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }

    #[test]
    fn matches_only_exact_authorities() {
        assert!(authority_matches(
            "api.example.test",
            "api.example.test",
            443,
            "https"
        ));
        assert!(authority_matches(
            "api.example.test:8443",
            "api.example.test",
            8443,
            "https"
        ));
        assert!(!authority_matches(
            "example.test",
            "api.example.test",
            443,
            "https"
        ));
        assert!(!authority_matches("127.0.0.1", "127.0.0.1", 80, "http"));
        assert!(authority_matches("127.0.0.1:80", "127.0.0.1", 80, "http"));
    }

    #[test]
    fn rejects_private_and_special_addresses() {
        for ip in [
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            "fd00::1".parse().expect("valid fixture"),
            "2001:db8::1".parse().expect("valid fixture"),
            "2002:7f00:1::1".parse().expect("valid fixture"),
            "3fff::1".parse().expect("valid fixture"),
            "64:ff9b::127.0.0.1".parse().expect("valid fixture"),
        ] {
            assert!(is_forbidden_public_destination(ip), "{ip}");
        }
        assert!(!is_forbidden_public_destination(
            "93.184.216.34".parse().expect("valid fixture")
        ));
        assert!(!is_forbidden_public_destination(
            "2606:4700:4700::1111".parse().expect("valid fixture")
        ));
    }

    #[test]
    fn rejects_zero_or_overbroad_configuration() {
        let error = BufferedHttpClient::disabled(
            HttpHostCeilings {
                max_headers: 0,
                ..HttpHostCeilings::default()
            },
            Duration::from_secs(1),
        )
        .expect_err("zero native ceiling must fail");
        assert_eq!(
            error,
            ConfigurationError::ZeroCeiling {
                field: "max_headers"
            }
        );

        let error = BufferedHttpClient::authorized(
            HttpConstraints {
                max_requests: 33,
                ..grant("api.example.test".to_owned(), "GET")
            },
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect_err("grant cannot exceed native ceiling");
        assert_eq!(error, ConfigurationError::GrantExceedsCeiling);

        let error = BufferedHttpClient::disabled(HttpHostCeilings::default(), Duration::MAX)
            .expect_err("unrepresentable runtime deadline must fail");
        assert_eq!(error, ConfigurationError::TimeoutOverflow);
    }

    /// An entry this host cannot match is a grant it must refuse, not one it accepts and then
    /// denies on every call. The rule set is shared with the broker that issues the grant.
    #[test]
    fn rejects_grant_entries_no_authority_can_match() {
        for host in [" api.example.test", "*", "a/b", ""] {
            let error = BufferedHttpClient::authorized(
                grant(host.to_owned(), "GET"),
                HttpHostCeilings::default(),
                Duration::from_secs(1),
            )
            .expect_err("an inexact host must not configure a client");
            assert_eq!(
                error,
                ConfigurationError::InvalidGrant {
                    source: HttpConstraintsError::InvalidHost {
                        value: host.to_owned()
                    }
                }
            );
        }

        let error = BufferedHttpClient::authorized(
            grant("api.example.test".to_owned(), "GET POST"),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect_err("an inexact method must not configure a client");
        assert_eq!(
            error,
            ConfigurationError::InvalidGrant {
                source: HttpConstraintsError::InvalidMethod {
                    value: "GET POST".to_owned()
                }
            }
        );
    }

    #[test]
    fn bounds_resolver_results_before_client_construction() {
        let addresses = (1..=17).map(|last| SocketAddr::from(([192, 0, 2, last], 443)));
        let error = bounded_addresses(addresses).expect_err("resolver fan-out must be bounded");
        assert_eq!(error.code, ErrorCode::Dns);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sends_extension_methods_duplicate_headers_and_buffered_bodies() {
        let (authority, recorded, server) = mock_http(
            b"HTTP/1.1 200 OK\r\nX-Value: one\r\nX-Value: two\r\nSet-Cookie: secret=session\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let mut client = BufferedHttpClient::authorized(
            grant(authority.clone(), "PROPFIND"),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        let response = client
            .send(Request {
                method: "PROPFIND".to_owned(),
                uri: format!("http://{authority}/items?private=no"),
                headers: vec![
                    Header {
                        name: "x-probe".to_owned(),
                        value: b"one".to_vec(),
                    },
                    Header {
                        name: "x-probe".to_owned(),
                        value: b"two".to_vec(),
                    },
                ],
                body: b"payload".to_vec(),
            })
            .await
            .expect("bounded request succeeds");

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
        assert_eq!(
            response
                .headers
                .iter()
                .filter(|header| header.name == "x-value")
                .count(),
            2
        );
        assert!(
            response
                .headers
                .iter()
                .all(|header| header.name != "set-cookie")
        );
        let request = recorded.recv().expect("request recorded");
        assert!(request.starts_with(b"PROPFIND /items?private=no HTTP/1.1\r\n"));
        assert!(request.ends_with(b"\r\n\r\npayload"));
        let evidence = client.into_evidence();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].authority, authority);
        server.join().expect("fixture server exits");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn denies_other_destinations_and_sensitive_request_headers() {
        let mut client = BufferedHttpClient::authorized(
            grant("127.0.0.1:10".to_owned(), "GET"),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "http://127.0.0.1:9/".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("different authority must fail before connection");
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(client.policy_violation(), Some("denied"));

        let mut client = BufferedHttpClient::authorized(
            grant("127.0.0.1:9".to_owned(), "GET"),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "http://127.0.0.1:9/".to_owned(),
                headers: vec![Header {
                    name: "authorization".to_owned(),
                    value: b"Bearer secret".to_vec(),
                }],
                body: Vec::new(),
            })
            .await
            .expect_err("guest authorization header must fail before connection");
        assert_eq!(error.code, ErrorCode::InvalidHeader);
        assert_eq!(client.policy_violation(), Some("invalid-http-request"));
    }

    fn credential_for(authority: &str) -> BoundCredential {
        BoundCredential::bearer(
            "Bearer",
            Redacted::new("fixture-secret-value".to_owned()),
            vec![authority.to_owned()],
        )
        .expect("valid fixture credential")
    }

    #[test]
    fn bound_credentials_fail_closed_on_structural_problems() {
        let secret = || Redacted::new("secret".to_owned());
        let destinations = || vec!["api.example.test".to_owned()];
        for (scheme, secret, destinations) in [
            ("", secret(), destinations()),
            ("Bea rer", secret(), destinations()),
            ("Bearer", Redacted::new(String::new()), destinations()),
            ("Bearer", Redacted::new("x".repeat(4097)), destinations()),
            (
                "Bearer",
                Redacted::new("bad\r\nheader".to_owned()),
                destinations(),
            ),
            ("Bearer", secret(), Vec::new()),
            (
                "Bearer",
                secret(),
                vec!["https://api.example.test/path".to_owned()],
            ),
            ("Bearer", secret(), vec!["*.example.test".to_owned()]),
        ] {
            assert!(matches!(
                BoundCredential::bearer(scheme, secret, destinations),
                Err(ConfigurationError::InvalidCredential { .. })
            ));
        }
        // The one thing a credential error must never carry is the secret itself.
        let error = BoundCredential::bearer(
            "Bearer",
            Redacted::new("tell-nobody\n".to_owned()),
            destinations(),
        )
        .expect_err("control bytes are refused");
        assert!(!error.to_string().contains("tell-nobody"), "{error}");
    }

    #[test]
    fn bound_credentials_never_render_their_value() {
        let credential = credential_for("api.example.test");
        let debug = format!("{credential:?}");
        assert!(!debug.contains("fixture-secret-value"), "{debug}");
        assert!(debug.contains("api.example.test"), "{debug}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn injects_a_destination_bound_credential_after_guest_checks() {
        let response: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        // Twin requests, one credentialed and one not, so the accounting assertion below compares
        // like with like: the injected header must change neither the guest-authored byte count
        // nor anything else the evidence reports.
        let (plain_authority, plain_recorded, plain_server) = mock_http(response);
        let (authority, recorded, server) = mock_http(response);

        let request_to = |authority: &str| Request {
            method: "GET".to_owned(),
            uri: format!("http://{authority}/pulls/7"),
            headers: vec![Header {
                name: "x-probe".to_owned(),
                value: b"one".to_vec(),
            }],
            body: Vec::new(),
        };

        let mut plain = BufferedHttpClient::authorized(
            grant(plain_authority.clone(), "GET"),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        plain
            .send(request_to(&plain_authority))
            .await
            .expect("uncredentialed request succeeds");

        let mut credentialed = BufferedHttpClient::authorized_with_credential(
            grant(authority.clone(), "GET"),
            Some(credential_for(&authority)),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        credentialed
            .send(request_to(&authority))
            .await
            .expect("credentialed request succeeds");

        let plain_request = String::from_utf8(plain_recorded.recv().expect("plain recorded"))
            .expect("fixture request is UTF-8");
        assert!(
            !plain_request.to_ascii_lowercase().contains("authorization"),
            "{plain_request}"
        );
        let request = String::from_utf8(recorded.recv().expect("request recorded"))
            .expect("fixture request is UTF-8");
        assert!(
            request.contains("authorization: Bearer fixture-secret-value"),
            "{request}"
        );

        let plain_evidence = plain.into_evidence();
        let evidence = credentialed.into_evidence();
        assert!(!plain_evidence[0].credential_injected);
        assert!(evidence[0].credential_injected);
        // Guest-authored accounting is identical: the credential is invisible to byte grants,
        // so its length can neither starve a request nor leak into public evidence. The twin
        // requests differ only by their fixture authorities, so normalize for that before
        // comparing.
        let authority_delta = authority.len() as i64 - plain_authority.len() as i64;
        assert_eq!(
            evidence[0].request_bytes as i64 - authority_delta,
            plain_evidence[0].request_bytes as i64
        );

        plain_server.join().expect("plain fixture exits");
        server.join().expect("fixture server exits");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_destinations_outside_the_credential_binding() {
        // The grant allows the destination; the credential does not. Sending without the
        // credential would be the quiet failure mode, so the request must not happen at all.
        let mut client = BufferedHttpClient::authorized_with_credential(
            grant("127.0.0.1:9".to_owned(), "GET"),
            Some(credential_for("other.example.test")),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "http://127.0.0.1:9/".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("unbound destinations are refused, not sent unauthenticated");
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(client.policy_violation(), Some("denied"));
        assert!(client.into_evidence().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn guest_authorization_is_rejected_not_overwritten_when_a_credential_exists() {
        // Silently replacing a guest-supplied credential would train providers to attempt one.
        let mut client = BufferedHttpClient::authorized_with_credential(
            grant("127.0.0.1:9".to_owned(), "GET"),
            Some(credential_for("127.0.0.1:9")),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "http://127.0.0.1:9/".to_owned(),
                headers: vec![Header {
                    name: "authorization".to_owned(),
                    value: b"Bearer guest-supplied".to_vec(),
                }],
                body: Vec::new(),
            })
            .await
            .expect_err("guest authorization must still be rejected");
        assert_eq!(error.code, ErrorCode::InvalidHeader);
    }

    #[test]
    fn credential_evidence_flag_defaults_for_old_records_and_serializes_compactly() {
        let old_record = r#"{"method":"GET","authority":"api.example.test:443","requestBytes":10,"responseBytes":20}"#;
        let evidence =
            serde_json::from_str::<HttpCallEvidence>(old_record).expect("old records still parse");
        assert!(!evidence.credential_injected);
        // False stays absent so pre-credential chains and digests are byte-identical.
        let serialized = serde_json::to_string(&evidence).expect("serializes");
        assert!(!serialized.contains("credentialInjected"), "{serialized}");

        let injected = HttpCallEvidence {
            credential_injected: true,
            ..evidence
        };
        let serialized = serde_json::to_string(&injected).expect("serializes");
        assert!(
            serialized.contains("\"credentialInjected\":true"),
            "{serialized}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn streams_into_response_bound_and_never_follows_redirects() {
        let body = "x".repeat(512);
        let oversized = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (authority, _recorded, server) = mock_http(oversized.as_bytes());
        let mut limited_grant = grant(authority.clone(), "GET");
        limited_grant.max_response_bytes = 128;
        let mut client = BufferedHttpClient::authorized(
            limited_grant,
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: format!("http://{authority}/large"),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("response bound must stop buffering");
        assert_eq!(error.code, ErrorCode::ResponseTooLarge);
        assert_eq!(client.policy_violation(), Some("byte-limit"));
        server.join().expect("fixture server exits");

        let (authority, _recorded, server) = mock_http(
            b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let mut client = BufferedHttpClient::authorized(
            grant(authority.clone(), "GET"),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        let response = client
            .send(Request {
                method: "GET".to_owned(),
                uri: format!("http://{authority}/redirect"),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect("redirect response is returned without following");
        assert_eq!(response.status, 302);
        assert_eq!(client.into_evidence().len(), 1);
        server.join().expect("fixture server exits");
    }

    #[test]
    fn bounds_and_sanitizes_error_messages() {
        let message = bounded_message(&format!("line one\n{}", "x".repeat(400)));
        assert!(message.len() <= 256);
        assert!(!message.contains('\n'));
    }
}
