//! Every response from a credentialed context is checked for the raw or encoded secret and refused
//! as Denied rather than returned to the component.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
pub mod asset;
mod stream;
pub use stream::{
    CHUNK_BYTES, FilePart, Part, Representation, StreamedRequest, StreamedResponse, read_decoded,
};

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    error::Error as _,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use dekopon_capability::{HttpConstraints, HttpConstraintsError, SecretUseGrant};
use dekopon_core::{Redacted, SecretBytes, SecretSinkKind};
use futures_util::StreamExt as _;
use reqwest::{
    Method, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue},
    redirect,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::{Instant, timeout};
use tracing::Instrument as _;

const DEFAULT_HTTPS_PORT: u16 = 443;
const CHATGPT_ACCOUNT_HEADER: &str = "chatgpt-account-id";
const MAX_ERROR_MESSAGE_BYTES: usize = 256;
const MAX_RESOLVED_ADDRESSES: usize = 16;
const REQUEST_ENCODING_OVERHEAD_BYTES: u64 = 128;

pub const DEFAULT_MAX_REQUESTS: u32 = 32;
pub const DEFAULT_MAX_REQUEST_BYTES: u64 = 1024 * 1024;
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_MAX_HEADERS: usize = 128;
pub const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_CREDENTIAL_BYTES: usize = 4096;
/// The minimum is 16 bytes because the echo check substring-searches every response, and a shorter
/// secret like token would falsely match unrelated text.
pub const MIN_CREDENTIAL_BYTES: usize = 16;
const _: () = assert!(MIN_CREDENTIAL_BYTES == 16);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Header {
    pub name: String,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub method: String,
    pub uri: String,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<Header>,
    pub body: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCode {
    InvalidMethod,
    InvalidUri,
    InvalidHeader,
    RequestTooLarge,
    Denied,
    HostCallLimit,
    Dns,
    Connect,
    Tls,
    Timeout,
    Protocol,
    ResponseTooLarge,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpError {
    pub code: ErrorCode,
    pub message: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for HttpError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlaintextHosts {
    hosts: Arc<BTreeSet<String>>,
}

impl PlaintextHosts {
    pub fn new<I>(entries: I) -> Result<Self, PlaintextHostError>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut hosts = BTreeSet::new();
        for entry in entries {
            let entry = entry.as_ref().trim();
            if entry.is_empty() {
                return Err(PlaintextHostError::Empty);
            }
            let owned = || entry.to_owned();
            if entry.contains("://") {
                return Err(PlaintextHostError::Scheme { entry: owned() });
            }
            if entry.contains('*') {
                return Err(PlaintextHostError::Wildcard { entry: owned() });
            }
            if entry.contains('/') {
                return Err(PlaintextHostError::Path { entry: owned() });
            }
            if entry.contains(':') {
                return Err(PlaintextHostError::Port { entry: owned() });
            }
            if !is_bare_hostname(entry) {
                return Err(PlaintextHostError::InvalidHost { entry: owned() });
            }
            hosts.insert(entry.to_ascii_lowercase());
        }
        Ok(Self {
            hosts: Arc::new(hosts),
        })
    }

    #[must_use]
    pub fn contains(&self, host: &str) -> bool {
        self.hosts.contains(host.to_ascii_lowercase().as_str())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> {
        self.hosts.iter().map(String::as_str)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PlaintextHostError {
    #[error("a plaintext host entry is empty")]
    Empty,
    #[error("plaintext host `{entry}` must be a bare hostname, without a scheme")]
    Scheme { entry: String },
    #[error("plaintext host `{entry}` must be a bare hostname, without a path")]
    Path { entry: String },
    #[error("plaintext host `{entry}` must carry no port: the port is irrelevant to this rule")]
    Port { entry: String },
    #[error("plaintext host `{entry}` must be an exact hostname: wildcards are not accepted")]
    Wildcard { entry: String },
    #[error("plaintext host `{entry}` is not a hostname")]
    InvalidHost { entry: String },
}

fn is_bare_hostname(entry: &str) -> bool {
    entry
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
        && !entry.starts_with(['-', '.'])
        && !entry.ends_with(['-', '.'])
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonPublicHttpsAuthority {
    pub authority: String,
}

impl NonPublicHttpsAuthority {
    pub fn new(authority: &str) -> Result<Self, &'static str> {
        let url = Url::parse(&format!("https://{authority}"))
            .map_err(|_error| "authority must be a DNS hostname and explicit port")?;
        let host = url.host_str().ok_or("authority must have a hostname")?;
        if url.username() != ""
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port().is_none()
            || !is_bare_hostname(host)
            || host.parse::<IpAddr>().is_ok()
            || !authority
                .eq_ignore_ascii_case(&format!("{host}:{}", url.port().unwrap_or_default()))
        {
            return Err("authority must be an exact DNS hostname and explicit port");
        }
        Ok(Self {
            authority: authority.to_ascii_lowercase(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpHostCeilings {
    pub max_requests: u32,
    pub max_request_bytes: u64,
    pub max_response_bytes: u64,
    pub max_headers: usize,
    pub max_header_bytes: usize,
    pub plaintext_hosts: PlaintextHosts,
    pub extra_ca_bundles: Arc<Vec<Vec<u8>>>,
    pub non_public_https: Arc<Vec<NonPublicHttpsAuthority>>,
}

impl Default for HttpHostCeilings {
    fn default() -> Self {
        Self {
            max_requests: DEFAULT_MAX_REQUESTS,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_headers: DEFAULT_MAX_HEADERS,
            max_header_bytes: DEFAULT_MAX_HEADER_BYTES,
            plaintext_hosts: PlaintextHosts::default(),
            extra_ca_bundles: Arc::new(Vec::new()),
            non_public_https: Arc::new(Vec::new()),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ConfigurationError {
    #[error("HTTP host ceiling {field} must be greater than zero")]
    ZeroCeiling { field: &'static str },
    #[error("HTTP authorization is not exact: {source}")]
    InvalidGrant {
        #[source]
        source: HttpConstraintsError,
    },
    #[error("HTTP authorization exceeds native host ceilings")]
    GrantExceedsCeiling,
    #[error("HTTP execution deadline must be greater than zero")]
    ZeroTimeout,
    #[error("HTTP execution deadline is too large")]
    TimeoutOverflow,
    #[error("secret-use grant is invalid")]
    InvalidSecretGrant {
        #[source]
        source: dekopon_capability::SecretUseGrantError,
    },
    #[error("bound credential is invalid: {reason}")]
    InvalidCredential { reason: &'static str },
}

/// This lets an agent use a credential without ever seeing it: the value stays in a Redacted
/// wrapper end to end, and a guest-supplied authorization header is rejected, never overwritten.
#[derive(Clone)]
pub struct BoundCredential {
    header_value: Redacted<String>,
    companion_header: Option<(HeaderName, Redacted<String>)>,
    destinations: Vec<String>,
    secret_binding: Option<SecretBindingIdentity>,
    echo_values: Vec<SecretBytes>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SecretBindingIdentity {
    grant: SecretUseGrant,
}

impl fmt::Debug for BoundCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundCredential")
            .field("header_value", &self.header_value)
            .field(
                "companion_header",
                &self
                    .companion_header
                    .as_ref()
                    .map(|(name, value)| (name, value)),
            )
            .field("destinations", &self.destinations)
            .field("secret_binding", &self.secret_binding.is_some())
            .field("echo_values", &self.echo_values)
            .finish()
    }
}

impl BoundCredential {
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
            if value.len() < MIN_CREDENTIAL_BYTES {
                return Err(invalid("secret is shorter than the 16-byte minimum"));
            }
            if value.len() > MAX_CREDENTIAL_BYTES {
                return Err(invalid("secret exceeds the credential byte limit"));
            }
            if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(invalid(
                    "secret contains whitespace, control, or non-ASCII bytes",
                ));
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
        let echo_value = SecretBytes::new(secret.expose().as_bytes().to_vec());
        let header_value = Redacted::new(format!("{scheme} {}", secret.expose()));
        Ok(Self {
            header_value,
            companion_header: None,
            destinations,
            secret_binding: None,
            echo_values: vec![echo_value],
        })
    }

    pub fn chatgpt_subscription(
        access: Redacted<String>,
        account_id: &str,
        destinations: Vec<String>,
    ) -> Result<Self, ConfigurationError> {
        let invalid = |reason| ConfigurationError::InvalidCredential { reason };
        if account_id.is_empty()
            || account_id.len() > MAX_CREDENTIAL_BYTES
            || !account_id.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(invalid(
                "ChatGPT account identifier must be a printable ASCII token",
            ));
        }
        let mut credential = Self::bearer("Bearer", access, destinations)?;
        credential.companion_header = Some((
            HeaderName::from_static(CHATGPT_ACCOUNT_HEADER),
            Redacted::new(account_id.to_owned()),
        ));
        Ok(credential)
    }

    pub fn secret_bearer(
        secret: SecretBytes,
        grant: &SecretUseGrant,
    ) -> Result<Self, ConfigurationError> {
        if grant.sink != SecretSinkKind::HttpBearer || grant.basic_username.is_some() {
            return Err(ConfigurationError::InvalidCredential {
                reason: "Bearer material does not match the secret-use sink",
            });
        }
        #[allow(
            clippy::map_err_ignore,
            reason = "Utf8Error carries the secret-derived invalid byte offset and valid prefix \
                      length; the fixed structural refusal must not expose either"
        )]
        let token = std::str::from_utf8(secret.expose()).map_err(|_| {
            ConfigurationError::InvalidCredential {
                reason: "Bearer secret must be UTF-8",
            }
        })?;
        if token.len() < MIN_CREDENTIAL_BYTES {
            return Err(ConfigurationError::InvalidCredential {
                reason: "Bearer secret is shorter than the 16-byte minimum",
            });
        }
        if token.len() > MAX_CREDENTIAL_BYTES
            || token
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(ConfigurationError::InvalidCredential {
                reason: "Bearer secret must be a bounded token without whitespace or controls",
            });
        }
        let rendered = format!("Bearer {token}");
        Ok(Self {
            header_value: Redacted::new(rendered),
            companion_header: None,
            destinations: grant.allowed_hosts.clone(),
            secret_binding: Some(SecretBindingIdentity {
                grant: grant.clone(),
            }),
            echo_values: vec![secret],
        })
    }

    pub fn secret_basic(
        password: SecretBytes,
        grant: &SecretUseGrant,
    ) -> Result<Self, ConfigurationError> {
        let Some(username) = grant.basic_username.as_deref() else {
            return Err(ConfigurationError::InvalidCredential {
                reason: "Basic secret use requires a fixed username",
            });
        };
        if password.expose().len() < MIN_CREDENTIAL_BYTES {
            return Err(ConfigurationError::InvalidCredential {
                reason: "Basic secret is shorter than the 16-byte minimum",
            });
        }
        if grant.sink != SecretSinkKind::HttpBasic
            || username.is_empty()
            || username.contains(':')
            || username.bytes().any(|byte| byte.is_ascii_control())
            || password.expose().len() > MAX_CREDENTIAL_BYTES
            || password.expose().contains(&b'\n')
            || password.expose().contains(&b'\r')
        {
            return Err(ConfigurationError::InvalidCredential {
                reason: "Basic credential fields are structurally invalid",
            });
        }
        let mut pair = Vec::with_capacity(username.len() + 1 + password.expose().len());
        pair.extend_from_slice(username.as_bytes());
        pair.push(b':');
        pair.extend_from_slice(password.expose());
        let encoded = STANDARD.encode(&pair);
        let rendered = format!("Basic {encoded}");
        Ok(Self {
            header_value: Redacted::new(rendered),
            companion_header: None,
            destinations: grant.allowed_hosts.clone(),
            secret_binding: Some(SecretBindingIdentity {
                grant: grant.clone(),
            }),
            echo_values: vec![password, SecretBytes::new(encoded.into_bytes())],
        })
    }

    #[must_use]
    pub fn destinations(&self) -> &[String] {
        &self.destinations
    }

    #[must_use]
    pub fn covers(&self, allowed_host: &str) -> bool {
        destinations_cover(&self.destinations, allowed_host)
    }

    fn matches(&self, host: &str, port: u16, scheme: &str) -> bool {
        self.destinations
            .iter()
            .any(|destination| authority_matches(destination, host, port, scheme))
    }

    #[must_use]
    pub fn matches_secret_grant(&self, grant: Option<&SecretUseGrant>) -> bool {
        match (&self.secret_binding, grant) {
            (None, None) => true,
            (Some(identity), Some(grant)) => &identity.grant == grant,
            _ => false,
        }
    }

    fn echoes_stream_chunk(&self, window: &mut Vec<u8>, bytes: &[u8], overlap: usize) -> bool {
        window.extend_from_slice(bytes);
        if self
            .echo_values
            .iter()
            .any(|secret| !secret.expose().is_empty() && contains_bytes(window, secret.expose()))
        {
            return true;
        }
        let keep = window.len().min(overlap);
        let start = window.len() - keep;
        window.copy_within(start.., 0);
        window.truncate(keep);
        false
    }

    fn echoes_credential(&self, response: &Response) -> bool {
        self.echo_values.iter().any(|secret| {
            let secret = secret.expose();
            !secret.is_empty()
                && (contains_bytes(&response.body, secret)
                    || response.headers.iter().any(|header| {
                        contains_bytes(header.name.as_bytes(), secret)
                            || contains_bytes(&header.value, secret)
                    }))
        })
    }

    fn render(&self) -> Result<HeaderValue, HttpError> {
        #[allow(
            clippy::map_err_ignore,
            reason = "`InvalidHeaderValue` reports only that parsing failed, and `bearer` already \
                      restricted these bytes to ASCII graphic; nothing derived from a credential \
                      value may be reported anyway"
        )]
        let mut value = HeaderValue::from_str(self.header_value.expose())
            .map_err(|_| http_error(ErrorCode::Internal, "credential could not be rendered"))?;
        value.set_sensitive(true);
        Ok(value)
    }

    fn companion_name(&self) -> Option<&HeaderName> {
        self.companion_header.as_ref().map(|(name, _)| name)
    }

    fn render_companion(&self) -> Option<Result<(HeaderName, HeaderValue), HttpError>> {
        let (name, value) = self.companion_header.as_ref()?;
        #[allow(
            clippy::map_err_ignore,
            reason = "`InvalidHeaderValue` reports only that parsing failed, and the constructor \
                      already restricted these bytes to ASCII graphic; nothing derived from a \
                      credential value may be reported anyway"
        )]
        let rendered = HeaderValue::from_str(value.expose())
            .map_err(|_| http_error(ErrorCode::Internal, "credential could not be rendered"));
        Some(rendered.map(|mut rendered| {
            rendered.set_sensitive(true);
            (name.clone(), rendered)
        }))
    }
}

/// This must agree exactly with BoundCredential's own coverage check, since a divergence would let
/// the policy layer accept a host the runtime injector then refuses.
#[must_use]
pub fn destinations_cover(destinations: &[String], allowed_host: &str) -> bool {
    let allowed = allowed_host.trim().to_ascii_lowercase();
    destinations
        .iter()
        .any(|destination| destination.trim().to_ascii_lowercase() == allowed)
}

fn is_destination_scope(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        && !value.contains(['/', '?', '#', '@', '*'])
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpCallEvidence {
    pub method: String,
    pub authority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// request_bytes deliberately excludes any broker-injected credential header, since its length
    /// must not leak into evidence or cost the guest's byte grant.
    pub request_bytes: u64,
    pub response_bytes: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub credential_injected: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

fn contains_bytes(bytes: &[u8], value: &[u8]) -> bool {
    !value.is_empty() && bytes.windows(value.len()).any(|window| window == value)
}

fn secret_raw_path_is_ambiguous(uri: &str) -> bool {
    let Some((_, authority_and_path)) = uri.split_once("://") else {
        return true;
    };
    if authority_and_path.contains('\\') {
        return true;
    }
    let raw_path = authority_and_path
        .find('/')
        .map_or("/", |index| &authority_and_path[index..]);
    let raw_path = raw_path.split_once('?').map_or(raw_path, |(path, _)| path);
    let raw_path = raw_path.split_once('#').map_or(raw_path, |(path, _)| path);
    raw_path.contains('%')
        || raw_path.contains("//")
        || raw_path
            .split('/')
            .skip(1)
            .any(|segment| matches!(segment, "." | ".."))
}

#[derive(Debug)]
pub struct BufferedHttpClient {
    grant: Option<HttpConstraints>,
    secret_grant: Option<SecretUseGrant>,
    credential: Option<BoundCredential>,
    ceilings: HttpHostCeilings,
    deadline: Instant,
    calls: u32,
    secret_injections: u32,
    attempted: bool,
    policy_violation: Option<&'static str>,
    asset_over_budget: bool,
    evidence: Vec<HttpCallEvidence>,
    resolved: HashMap<String, Vec<SocketAddr>>,
    pinned_client: Option<PinnedClient>,
}

#[derive(Debug)]
struct PinnedClient {
    host: String,
    addresses: Vec<SocketAddr>,
    client: reqwest::Client,
}

impl BufferedHttpClient {
    pub fn disabled(
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        validate_configuration(None, &ceilings, timeout)?;
        Self::new(None, None, None, ceilings, timeout)
    }

    pub fn authorized(
        grant: HttpConstraints,
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        validate_configuration(Some(&grant), &ceilings, timeout)?;
        Self::new(Some(grant), None, None, ceilings, timeout)
    }

    pub fn authorized_with_credential(
        grant: HttpConstraints,
        credential: Option<BoundCredential>,
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        validate_configuration(Some(&grant), &ceilings, timeout)?;
        if credential
            .as_ref()
            .is_some_and(|credential| !credential.matches_secret_grant(None))
        {
            return Err(ConfigurationError::InvalidCredential {
                reason: "DRN-bound credential requires an authorization-bound secret grant",
            });
        }
        Self::new(Some(grant), None, credential, ceilings, timeout)
    }

    pub fn authorized_with_secret_credential(
        grant: HttpConstraints,
        secret_grant: SecretUseGrant,
        credential: BoundCredential,
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        validate_configuration(Some(&grant), &ceilings, timeout)?;
        secret_grant
            .validate()
            .map_err(|source| ConfigurationError::InvalidSecretGrant { source })?;
        if !credential.matches_secret_grant(Some(&secret_grant)) {
            return Err(ConfigurationError::InvalidCredential {
                reason: "resolved credential does not match the authorization-bound secret grant",
            });
        }
        Self::new(
            Some(grant),
            Some(secret_grant),
            Some(credential),
            ceilings,
            timeout,
        )
    }

    fn new(
        grant: Option<HttpConstraints>,
        secret_grant: Option<SecretUseGrant>,
        credential: Option<BoundCredential>,
        ceilings: HttpHostCeilings,
        timeout: Duration,
    ) -> Result<Self, ConfigurationError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(ConfigurationError::TimeoutOverflow)?;
        Ok(Self {
            grant,
            secret_grant,
            credential,
            ceilings,
            deadline,
            calls: 0,
            secret_injections: 0,
            attempted: false,
            policy_violation: None,
            asset_over_budget: false,
            evidence: Vec::new(),
            resolved: HashMap::new(),
            pinned_client: None,
        })
    }

    pub fn asset_over_budget(&self) -> bool {
        self.asset_over_budget
    }

    pub fn attempted(&self) -> bool {
        self.attempted
    }

    pub fn policy_violation(&self) -> Option<&'static str> {
        self.policy_violation
    }

    pub fn into_evidence(self) -> Vec<HttpCallEvidence> {
        self.evidence
    }

    pub async fn send(&mut self, request: Request) -> Result<Response, HttpError> {
        let span = tracing::info_span!(
            "http.request",
            "http.request.method" = tracing::field::Empty,
            "server.address" = tracing::field::Empty,
            "http.response.status_code" = tracing::field::Empty,
            "dekopon.http.request.accounted_bytes" = tracing::field::Empty,
            "dekopon.http.response.accounted_bytes" = tracing::field::Empty,
            "error.code" = tracing::field::Empty,
            "error.message" = tracing::field::Empty,
            "url.full" = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        span.record("url.full", request.uri.as_str());

        let evidence_index = self.evidence.len();
        self.attempted = true;
        // This must use instrument, not enter: holding an entered guard across these awaits would
        // leave the span current on whatever thread happens to poll unrelated tasks.
        let result = self.send_checked(request).instrument(span.clone()).await;

        self.record_request(&span, evidence_index, &result);
        result
    }

    fn record_request<T>(
        &mut self,
        span: &tracing::Span,
        evidence_index: usize,
        result: &Result<T, HttpError>,
    ) {
        let outcome = outcome_label(result);
        let failure = result.as_ref().err();
        span.in_scope(|| {
            let evidence = self.evidence.get(evidence_index);
            if let Some(evidence) = evidence {
                span.record("http.request.method", evidence.method.as_str());
                span.record("server.address", evidence.authority.as_str());
                span.record(
                    "dekopon.http.request.accounted_bytes",
                    evidence.request_bytes,
                );
                span.record(
                    "dekopon.http.response.accounted_bytes",
                    evidence.response_bytes,
                );
                if let Some(status) = evidence.status {
                    span.record("http.response.status_code", status);
                }
            }
            if let Some(error) = failure {
                // Recording the error reason is safe only because every message this crate produces
                // is a static, pre-sanitized string; that property must be preserved.
                span.record("error.code", tracing::field::debug(error.code));
                span.record("error.message", error.message.as_str());
            }
            span.record("outcome", outcome);

            tracing::info!(
                target: "dekopon_http_host::audit",
                {
                    audit.event = "accounting.http.request",
                    "http.request.method" = evidence.map(|evidence| evidence.method.as_str()),
                    "server.address" = evidence.map(|evidence| evidence.authority.as_str()),
                    "http.response.status_code" = evidence.and_then(|evidence| evidence.status),
                    "dekopon.http.request.accounted_bytes" =
                        evidence.map(|evidence| evidence.request_bytes),
                    "dekopon.http.response.accounted_bytes" =
                        evidence.map(|evidence| evidence.response_bytes),
                    "error.code" = failure.map(|error| tracing::field::debug(error.code)),
                    "error.message" = failure.map(|error| error.message.as_str()),
                    outcome = outcome,
                },
                "external HTTP request accounted"
            );
        });

        if let Some(error) = failure {
            self.policy_violation = violation_label(error.code).or(self.policy_violation);
        }
    }

    async fn authorize_request(
        &mut self,
        request: Request,
        streamed: Option<stream::RequestLengths>,
    ) -> Result<(PreparedRequest, HttpConstraints, usize), HttpError> {
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

        if self.secret_grant.is_some() && secret_raw_path_is_ambiguous(&request.uri) {
            return Err(http_error(
                ErrorCode::Denied,
                "secret-bearing request path uses prohibited encoding or separators",
            ));
        }
        let mut prepared = self.prepare(request, &grant, streamed).await?;
        // Credential injection happens strictly after every guest-facing check, and its binding is
        // narrower than the grant and fails closed: an allowed-but-unbound destination is refused
        // rather than sent unauthenticated.
        if let Some(secret) = &self.secret_grant {
            let path_allowed = secret
                .allowed_paths
                .iter()
                .any(|rule| rule.matches(prepared.url.path()));
            let method_allowed = secret
                .allowed_methods
                .iter()
                .any(|method| method == prepared.method.as_str());
            let port = prepared
                .url
                .port_or_known_default()
                .unwrap_or(DEFAULT_HTTPS_PORT);
            let host_allowed = secret.allowed_hosts.iter().any(|authority| {
                authority_matches(authority, &prepared.host, port, prepared.url.scheme())
            });
            if !path_allowed
                || !method_allowed
                || !host_allowed
                || (!secret.allow_query && prepared.url.query().is_some())
            {
                return Err(http_error(
                    ErrorCode::Denied,
                    "request is outside this secret's authorized HTTP scope",
                ));
            }
            if self.secret_injections >= secret.max_injections {
                return Err(http_error(
                    ErrorCode::HostCallLimit,
                    "the authorized secret injection limit is exhausted",
                ));
            }
        }

        let credential_header = self.credential.as_ref().map(|credential| {
            let port = prepared
                .url
                .port_or_known_default()
                .unwrap_or(DEFAULT_HTTPS_PORT);
            if credential.matches(&prepared.host, port, prepared.url.scheme()) {
                credential.render()
            } else {
                Err(http_error(
                    ErrorCode::Denied,
                    "destination is outside this credential's binding",
                ))
            }
        });

        // Evidence is pushed before the credential decision so a binding refusal still leaves an
        // accounted, status-less record rather than a silent gap.
        let evidence_index = self.evidence.len();
        self.evidence.push(HttpCallEvidence {
            method: prepared.method.as_str().to_owned(),
            authority: prepared.authority.clone(),
            status: None,
            request_bytes: prepared.request_bytes,
            response_bytes: 0,
            credential_injected: false,
        });
        if let Some(header) = credential_header {
            prepared.headers.insert(AUTHORIZATION, header?);
            // The companion header is inserted only after accounting, alongside the bearer token,
            // so it never counts against the guest's byte grant.
            if let Some(companion) = self
                .credential
                .as_ref()
                .and_then(BoundCredential::render_companion)
            {
                let (name, value) = companion?;
                prepared.headers.insert(name, value);
            }
            self.evidence[evidence_index].credential_injected = true;
            if self.secret_grant.is_some() {
                self.secret_injections = self.secret_injections.saturating_add(1);
            }
        }

        if grant.propagate_trace
            && let Some(context) = dekopon_telemetry::current_trace_context()
        {
            let value = format!(
                "00-{:032x}-{:016x}-{:02x}",
                u128::from_be_bytes(context.trace_id),
                u64::from_be_bytes(context.span_id),
                context.flags,
            );
            let header = HeaderValue::from_str(&value).map_err(|_error| {
                http_error(ErrorCode::Internal, "trace context could not be rendered")
            })?;
            prepared.headers.insert("traceparent", header);
        }

        Ok((prepared, grant, evidence_index))
    }

    async fn send_checked(&mut self, request: Request) -> Result<Response, HttpError> {
        let (prepared, grant, evidence_index) = self.authorize_request(request, None).await?;
        let executed = self.execute(prepared, &grant).await;
        if let Ok((response, response_bytes)) = &executed {
            let evidence = &mut self.evidence[evidence_index];
            evidence.status = Some(response.status);
            evidence.response_bytes = *response_bytes;
        }
        let result = executed.and_then(|(response, bytes)| {
            if self
                .credential
                .as_ref()
                .is_some_and(|credential| credential.echoes_credential(&response))
            {
                Err(http_error(
                    ErrorCode::Denied,
                    "credentialed response echoed the credential",
                ))
            } else {
                Ok((response, bytes))
            }
        });
        result.map(|(response, _bytes)| response)
    }

    async fn prepare(
        &mut self,
        request: Request,
        grant: &HttpConstraints,
        streamed: Option<stream::RequestLengths>,
    ) -> Result<PreparedRequest, HttpError> {
        #[allow(
            clippy::map_err_ignore,
            reason = "`http::method::InvalidMethod` is an opaque unit error whose whole content is \
                      \"invalid HTTP method\", which the replacement already states"
        )]
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
        let bounded_body_bytes =
            streamed.map_or(request.body.len() as u64, |lengths| lengths.literal);
        let minimum_request_bytes =
            encoded_request_bytes(method.as_str(), &request.uri, 0, bounded_body_bytes)
                .ok_or_else(|| http_error(ErrorCode::RequestTooLarge, "request size overflowed"))?;
        if minimum_request_bytes > grant.max_request_bytes {
            return Err(http_error(
                ErrorCode::RequestTooLarge,
                "request exceeds the authorized byte limit",
            ));
        }

        let url = Url::parse(&request.uri).map_err(|error| {
            http_error(
                ErrorCode::InvalidUri,
                format!("URI is not a valid absolute URL: {error}"),
            )
        })?;
        if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
            return Err(http_error(
                ErrorCode::InvalidUri,
                "URI user information and fragments are prohibited",
            ));
        }
        // host_str() returns IPv6 addresses bracketed, but resolution, pinning, and credential
        // matching all require the bare unbracketed form.
        let host = unbracketed_host(
            url.host_str()
                .ok_or_else(|| http_error(ErrorCode::InvalidUri, "URI has no host"))?,
        )
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
        let companion = self
            .credential
            .as_ref()
            .and_then(BoundCredential::companion_name)
            .cloned();
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
            #[allow(
                clippy::map_err_ignore,
                reason = "`InvalidHeaderName` is an opaque unit error naming neither the offending \
                          byte nor its position"
            )]
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| {
                http_error(ErrorCode::InvalidHeader, "request header name is invalid")
            })?;
            if is_forbidden_request_header(&name, companion.as_ref()) {
                return Err(http_error(
                    ErrorCode::InvalidHeader,
                    "request header is broker-owned or hop-by-hop",
                ));
            }
            #[allow(
                clippy::map_err_ignore,
                reason = "`InvalidHeaderValue` is an opaque unit error naming neither the \
                          offending byte nor its position"
            )]
            let value = HeaderValue::from_bytes(&header.value).map_err(|_| {
                http_error(ErrorCode::InvalidHeader, "request header value is invalid")
            })?;
            headers.append(name, value);
        }

        let request_bytes = encoded_request_bytes(
            method.as_str(),
            url.as_str(),
            header_bytes,
            bounded_body_bytes,
        )
        .ok_or_else(|| http_error(ErrorCode::RequestTooLarge, "request size overflowed"))?;
        if request_bytes > grant.max_request_bytes {
            return Err(http_error(
                ErrorCode::RequestTooLarge,
                "request exceeds the authorized byte limit",
            ));
        }

        // DNS resolution is cached for this context's lifetime, but every cached address is still
        // validated on each call, so caching narrows latency, not what may be reached.
        let addresses = if let Some(addresses) = self.resolved.get(&authority) {
            addresses.clone()
        } else {
            let remaining = self.remaining()?;
            #[allow(
                clippy::map_err_ignore,
                reason = "`tokio::time::error::Elapsed` carries only \"deadline has elapsed\", which \
                          the Timeout message already states"
            )]
            let addresses = timeout(remaining, resolve_destination(&host, port))
                .await
                .map_err(|_| {
                    http_error(ErrorCode::Timeout, "destination resolution timed out")
                })??;
            self.resolved.insert(authority.clone(), addresses.clone());
            addresses
        };
        if !plaintext_permitted(
            url.scheme(),
            &host,
            &addresses,
            &self.ceilings.plaintext_hosts,
        ) {
            return Err(http_error(
                ErrorCode::Denied,
                format!(
                    "plaintext HTTP to {host} is refused: not a loopback destination, and not \
                     listed in the broker's http.plaintextHosts"
                ),
            ));
        }
        if url.scheme() == "https" {
            let non_public = self
                .ceilings
                .non_public_https
                .iter()
                .any(|entry| entry.authority == authority);
            if !https_addresses_permitted(non_public, &addresses) {
                return Err(http_error(
                    ErrorCode::Denied,
                    if non_public {
                        "non-public HTTPS requires private unicast addresses"
                    } else {
                        "destination resolved to a non-public address"
                    },
                ));
            }
        }

        Ok(PreparedRequest {
            method,
            url,
            headers,
            body: request.body,
            host,
            authority,
            addresses,
            request_bytes: streamed.map_or(request_bytes, |lengths| lengths.total),
        })
    }

    async fn execute(
        &mut self,
        request: PreparedRequest,
        grant: &HttpConstraints,
    ) -> Result<(Response, u64), HttpError> {
        let remaining = self.remaining()?;
        let client = self.pinned_client(&request.host, &request.addresses, remaining)?;

        #[allow(
            clippy::map_err_ignore,
            reason = "`tokio::time::error::Elapsed` carries only \"deadline has elapsed\", which \
                      the Timeout message already states"
        )]
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
            #[allow(
                clippy::map_err_ignore,
                reason = "`tokio::time::error::Elapsed` carries only \"deadline has elapsed\", \
                          which the Timeout message already states"
            )]
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

    fn pinned_client(
        &mut self,
        host: &str,
        addresses: &[SocketAddr],
        budget: Duration,
    ) -> Result<reqwest::Client, HttpError> {
        if let Some(pinned) = &self.pinned_client
            && pinned.host == host
            && pinned.addresses == addresses
        {
            return Ok(pinned.client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .no_proxy()
            .connect_timeout(budget)
            .timeout(budget)
            .resolve_to_addrs(host, addresses);
        for pem in self.ceilings.extra_ca_bundles.iter() {
            for cert in reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|_error| http_error(ErrorCode::Denied, "invalid configured CA bundle"))?
            {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder.build().map_err(|error| map_reqwest_error(&error))?;
        self.pinned_client = Some(PinnedClient {
            host: host.to_owned(),
            addresses: addresses.to_vec(),
            client: client.clone(),
        });
        Ok(client)
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
        .map_err(|error| {
            tracing::debug!(error = %error, "destination resolution failed");
            http_error(ErrorCode::Dns, "destination could not be resolved")
        })?;
    bounded_addresses(addresses)
}

/// The bound truncates rather than refuses because refusing at the limit would make a large
/// round-robin or dual-stack destination permanently or intermittently unreachable.
fn bounded_addresses(
    addresses: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, HttpError> {
    let mut unique = BTreeSet::new();
    for address in addresses {
        unique.insert(address);
        if unique.len() == MAX_RESOLVED_ADDRESSES {
            break;
        }
    }
    if unique.is_empty() {
        return Err(http_error(
            ErrorCode::Dns,
            "destination resolved to no addresses",
        ));
    }
    Ok(unique.into_iter().collect())
}

fn unbracketed_host(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}

fn canonical_host(host: &str) -> Cow<'_, str> {
    if host.contains(':') {
        Cow::Owned(format!("[{host}]"))
    } else {
        Cow::Borrowed(host)
    }
}

fn canonical_authority(host: &str, port: u16) -> String {
    format!("{}:{port}", canonical_host(host))
}

/// The loopback branch requires every resolved address to be loopback; the list matches by
/// hostname, not address, since address alone can't distinguish a LAN answer from a chosen one.
fn plaintext_permitted(
    scheme: &str,
    host: &str,
    addresses: &[SocketAddr],
    allowed: &PlaintextHosts,
) -> bool {
    scheme != "http"
        || addresses.iter().all(|address| address.ip().is_loopback())
        || allowed.contains(host)
}

fn authority_matches(allowed: &str, host: &str, port: u16, scheme: &str) -> bool {
    let allowed = allowed.trim().to_ascii_lowercase();
    allowed == canonical_authority(host, port)
        || (scheme == "https"
            && allowed == canonical_host(host).as_ref()
            && port == DEFAULT_HTTPS_PORT)
}

fn is_forbidden_request_header(name: &HeaderName, companion: Option<&HeaderName>) -> bool {
    if companion == Some(name) {
        return true;
    }
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
            | "traceparent"
            | "tracestate"
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

fn https_addresses_permitted(non_public: bool, addresses: &[SocketAddr]) -> bool {
    !addresses.is_empty()
        && addresses.iter().all(|address| {
            if non_public {
                is_private_unicast(address.ip())
            } else {
                !is_forbidden_public_destination(address.ip())
            }
        })
}

fn is_private_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() && !ip.is_loopback() && !ip.is_link_local(),
        IpAddr::V6(ip) => (ip.segments()[0] & 0xfe00) == 0xfc00,
    }
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
        || segments[0] == 0x2002
        || (segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
}

fn map_reqwest_error(error: &reqwest::Error) -> HttpError {
    if error.is_builder() {
        return http_error(ErrorCode::Internal, "native HTTP client setup failed");
    }
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

fn error_chain_contains(error: &reqwest::Error, fragments: &[&str]) -> bool {
    let mut source = error.source();
    while let Some(current) = source {
        let message = current.to_string().to_ascii_lowercase();
        if fragments.iter().any(|fragment| message.contains(fragment)) {
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

/// This match must stay exhaustive with no wildcard arm, or a new ErrorCode could silently disagree
/// across policy_violation, the span, and accounting.
fn violation_label(code: ErrorCode) -> Option<&'static str> {
    match code {
        ErrorCode::Denied => Some("denied"),
        ErrorCode::HostCallLimit => Some("host-call-limit"),
        ErrorCode::InvalidMethod | ErrorCode::InvalidUri | ErrorCode::InvalidHeader => {
            Some("invalid-http-request")
        }
        ErrorCode::RequestTooLarge | ErrorCode::ResponseTooLarge => Some("byte-limit"),
        ErrorCode::Dns
        | ErrorCode::Connect
        | ErrorCode::Tls
        | ErrorCode::Timeout
        | ErrorCode::Protocol
        | ErrorCode::Internal => None,
    }
}

fn outcome_label<T>(result: &Result<T, HttpError>) -> &'static str {
    match result {
        Ok(_) => "succeeded",
        Err(error) => violation_label(error.code).unwrap_or("failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        time::Duration,
    };

    use dekopon_test_support::LoopbackServer;

    use dekopon_capability::{HttpConstraints, HttpConstraintsError, HttpPathRule, SecretUseGrant};
    use dekopon_core::{Redacted, SecretBytes, SecretSinkKind};

    use super::{
        BoundCredential, BufferedHttpClient, ConfigurationError, ErrorCode, Header,
        HttpCallEvidence, HttpHostCeilings, MAX_RESOLVED_ADDRESSES, MIN_CREDENTIAL_BYTES,
        PlaintextHostError, PlaintextHosts, Request, authority_matches, bounded_addresses,
        bounded_message, is_forbidden_public_destination, map_reqwest_error, plaintext_permitted,
    };

    fn grant(authority: String, method: &str) -> HttpConstraints {
        HttpConstraints {
            allowed_hosts: vec![authority],
            allowed_methods: vec![method.to_owned()],
            max_requests: 2,
            max_request_bytes: 64 * 1024,
            max_response_bytes: 64 * 1024,
            allow_plaintext_loopback: true,
            propagate_trace: false,
        }
    }

    fn address(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().expect("valid fixture address"), port)
    }

    fn plaintext_hosts(entries: &[&str]) -> PlaintextHosts {
        PlaintextHosts::new(entries).expect("valid fixture host list")
    }

    #[test]
    fn plaintext_reaches_loopback_with_an_empty_allow_list() {
        let empty = PlaintextHosts::default();
        assert!(empty.is_empty());
        assert!(plaintext_permitted(
            "http",
            "localhost",
            &[address("127.0.0.1", 8080)],
            &empty
        ));
        assert!(plaintext_permitted(
            "http",
            "::1",
            &[address("::1", 8080)],
            &empty
        ));
        assert!(!plaintext_permitted(
            "http",
            "rpi.lan",
            &[address("192.168.1.20", 5080)],
            &empty
        ));
    }

    #[test]
    fn plaintext_reaches_a_listed_host_and_nothing_else() {
        let allowed = plaintext_hosts(&["rpi.lan", "openobserve.openobserve.svc"]);
        assert!(plaintext_permitted(
            "http",
            "rpi.lan",
            &[address("192.168.1.20", 5080)],
            &allowed
        ));
        assert!(plaintext_permitted(
            "http",
            "openobserve.openobserve.svc",
            &[address("10.43.7.9", 5080)],
            &allowed
        ));
        for refused in ["evil.rpi.lan", "rpi.lan.evil.test", "lan", "rpi"] {
            assert!(
                !plaintext_permitted("http", refused, &[address("192.168.1.20", 5080)], &allowed),
                "{refused} is not listed"
            );
        }
    }

    #[test]
    fn listed_plaintext_hosts_match_case_insensitively() {
        let allowed = plaintext_hosts(&["RPi.LAN"]);
        assert!(plaintext_permitted(
            "http",
            "rpi.lan",
            &[address("192.168.1.20", 5080)],
            &allowed
        ));
        assert!(allowed.contains("RPI.LAN"));
        assert_eq!(allowed.iter().collect::<Vec<_>>(), vec!["rpi.lan"]);
    }

    #[test]
    fn a_listed_host_does_not_relax_https_or_mixed_loopback_answers() {
        let allowed = plaintext_hosts(&["rpi.lan"]);
        assert!(plaintext_permitted(
            "https",
            "api.example.test",
            &[address("93.184.216.34", 443)],
            &allowed
        ));
        assert!(!plaintext_permitted(
            "http",
            "split.example.test",
            &[address("127.0.0.1", 80), address("93.184.216.34", 80)],
            &allowed
        ));
    }

    #[test]
    fn plaintext_host_entries_must_be_bare_hostnames() {
        for (entry, expected) in [
            ("", PlaintextHostError::Empty),
            ("   ", PlaintextHostError::Empty),
            (
                "http://rpi.lan",
                PlaintextHostError::Scheme {
                    entry: "http://rpi.lan".to_owned(),
                },
            ),
            (
                "rpi.lan/ingest",
                PlaintextHostError::Path {
                    entry: "rpi.lan/ingest".to_owned(),
                },
            ),
            (
                "rpi.lan:5080",
                PlaintextHostError::Port {
                    entry: "rpi.lan:5080".to_owned(),
                },
            ),
            (
                "::1",
                PlaintextHostError::Port {
                    entry: "::1".to_owned(),
                },
            ),
            (
                "*.lan",
                PlaintextHostError::Wildcard {
                    entry: "*.lan".to_owned(),
                },
            ),
            (
                "rpi lan",
                PlaintextHostError::InvalidHost {
                    entry: "rpi lan".to_owned(),
                },
            ),
            (
                "user@rpi.lan",
                PlaintextHostError::InvalidHost {
                    entry: "user@rpi.lan".to_owned(),
                },
            ),
            (
                ".rpi.lan",
                PlaintextHostError::InvalidHost {
                    entry: ".rpi.lan".to_owned(),
                },
            ),
            (
                "rpi.lan.",
                PlaintextHostError::InvalidHost {
                    entry: "rpi.lan.".to_owned(),
                },
            ),
        ] {
            let error = PlaintextHosts::new([entry]).expect_err("{entry} must be refused");
            assert_eq!(error, expected, "{entry}");
        }
        PlaintextHosts::new(["rpi.lan", "http://other.lan"])
            .expect_err("a single invalid entry must refuse the list");
        assert!(plaintext_hosts(&["  rpi.lan  "]).contains("rpi.lan"));
        assert!(plaintext_hosts(&["192.168.1.20"]).contains("192.168.1.20"));
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
    fn renders_ipv6_literal_authorities_with_exactly_one_pair_of_brackets() {
        assert!(authority_matches("[::1]:8080", "::1", 8080, "http"));
        assert!(authority_matches(
            "[2606:4700:4700::1111]",
            "2606:4700:4700::1111",
            443,
            "https"
        ));
        assert!(!authority_matches("[[::1]]:8080", "::1", 8080, "http"));
        assert!(!authority_matches("::1:8080", "::1", 8080, "http"));
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
    fn non_public_https_requires_exact_authority_independent_of_trust() {
        let profile = super::NonPublicHttpsAuthority::new(
            "OpenObserve-TLS.openobserve.svc.cluster.local:5443",
        )
        .expect("valid exact authority");
        assert_eq!(
            profile.authority,
            "openobserve-tls.openobserve.svc.cluster.local:5443"
        );
        for authority in [
            "openobserve-tls.openobserve.svc.cluster.local",
            "openobserve-tls.openobserve.svc.cluster.local:5443/path",
            "*.openobserve.svc.cluster.local:5443",
            "169.254.169.254:5443",
            "openobserve-tls.openobserve.svc.cluster.local:5443@evil.example:443",
        ] {
            assert!(
                super::NonPublicHttpsAuthority::new(authority).is_err(),
                "{authority}"
            );
        }
        for address in ["10.43.0.1", "172.16.0.2", "192.168.1.1", "fd00::1"] {
            assert!(
                super::is_private_unicast(address.parse().unwrap()),
                "{address}"
            );
        }
        for address in [
            "127.0.0.1",
            "169.254.169.254",
            "8.8.8.8",
            "::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(
                !super::is_private_unicast(address.parse().unwrap()),
                "{address}"
            );
        }
        let private = ["10.43.0.1:5443".parse().unwrap()];
        let mixed = [
            "10.43.0.1:5443".parse().unwrap(),
            "8.8.8.8:5443".parse().unwrap(),
        ];
        let public = ["8.8.8.8:443".parse().unwrap()];
        assert!(super::https_addresses_permitted(true, &private));
        assert!(!super::https_addresses_permitted(false, &private));
        assert!(!super::https_addresses_permitted(true, &mixed));
        assert!(!super::https_addresses_permitted(true, &public));
        assert!(super::https_addresses_permitted(false, &public));
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
    fn truncates_resolver_fan_out_instead_of_refusing_it() {
        let addresses = (1..=17).map(|last| SocketAddr::from(([192, 0, 2, last], 443)));
        let bounded =
            bounded_addresses(addresses).expect("a large fan-out is truncated, not refused");
        assert_eq!(bounded.len(), MAX_RESOLVED_ADDRESSES);

        let duplicated = (1..=20).map(|_| SocketAddr::from(([192, 0, 2, 1], 443)));
        let bounded = bounded_addresses(duplicated).expect("duplicates collapse before the bound");
        assert_eq!(bounded.len(), 1);

        let error = bounded_addresses(Vec::new()).expect_err("an empty answer is still a failure");
        assert_eq!(error.code, ErrorCode::Dns);
    }

    #[test]
    fn client_builder_failures_are_setup_failures_not_protocol_failures() {
        let error = reqwest::Client::new()
            .get("not an absolute url")
            .build()
            .expect_err("the builder rejects a non-absolute URL");
        assert!(error.is_builder());
        assert_eq!(map_reqwest_error(&error).code, ErrorCode::Internal);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sends_extension_methods_duplicate_headers_and_buffered_bodies() {
        let server = LoopbackServer::once(
            b"HTTP/1.1 200 OK\r\nX-Value: one\r\nX-Value: two\r\nSet-Cookie: secret=session\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let authority = server.authority().to_owned();
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
        let request = server.request();
        assert!(request.starts_with(b"PROPFIND /items?private=no HTTP/1.1\r\n"));
        assert!(request.ends_with(b"\r\n\r\npayload"));
        let evidence = client.into_evidence();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].authority, authority);
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_listed_plaintext_host_still_needs_the_authorization_to_allow_it() {
        let ceilings = HttpHostCeilings {
            plaintext_hosts: plaintext_hosts(&["api.example.test"]),
            ..HttpHostCeilings::default()
        };
        let mut client = BufferedHttpClient::authorized(
            HttpConstraints {
                allow_plaintext_loopback: false,
                ..grant("api.example.test:80".to_owned(), "GET")
            },
            ceilings.clone(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "http://api.example.test/".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("a constraint set that forbids plaintext still forbids it");
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(error.message, "plaintext HTTP is not authorized");

        let mut client = BufferedHttpClient::authorized(
            grant("other.example.test:80".to_owned(), "GET"),
            ceilings,
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "http://api.example.test/".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("a listed host the grant does not name is still not a destination");
        assert_eq!(error.code, ErrorCode::Denied);
        assert_eq!(
            error.message,
            "HTTP destination is not authorized for this invocation"
        );
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

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_uris_name_the_rule_they_broke() {
        let mut client = BufferedHttpClient::authorized(
            grant("api.example.test".to_owned(), "GET"),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");

        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "/relative/only".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("a relative reference is not an absolute URL");
        assert_eq!(error.code, ErrorCode::InvalidUri);
        assert!(
            error.message.contains("relative URL without a base"),
            "{error}"
        );

        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: "https://api.example.test:notaport/".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("a malformed port is not an absolute URL");
        assert_eq!(error.code, ErrorCode::InvalidUri);
        assert!(error.message.contains("invalid port number"), "{error}");
        assert!(!error.message.contains("api.example.test"), "{error}");
    }

    fn secret_grant(
        authority: &str,
        sink: SecretSinkKind,
        username: Option<&str>,
        path: &str,
    ) -> SecretUseGrant {
        SecretUseGrant {
            secret: "drn:com.xrl:secret:test:api/credential"
                .parse()
                .expect("canonical DRN"),
            sink,
            basic_username: username.map(str::to_owned),
            allowed_hosts: vec![authority.to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            allowed_paths: vec![HttpPathRule::Exact {
                path: path.to_owned(),
            }],
            allow_query: false,
            max_injections: 1,
            binding_id: "api-credential".to_owned(),
            map_revision: None,
        }
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
        let secret = || Redacted::new("fixture-secret-value".to_owned());
        let destinations = || vec!["api.example.test".to_owned()];
        for (scheme, secret, destinations) in [
            ("", secret(), destinations()),
            ("Bea rer", secret(), destinations()),
            ("Bearer", Redacted::new(String::new()), destinations()),
            ("Bearer", Redacted::new("x".repeat(15)), destinations()),
            (
                "Bearer",
                Redacted::new("phrase shaped secret".to_owned()),
                destinations(),
            ),
            (
                "Bearer",
                Redacted::new("tab\tseparated-secret".to_owned()),
                destinations(),
            ),
            ("Bearer", Redacted::new("x".repeat(4097)), destinations()),
            (
                "Bearer",
                Redacted::new("bad\r\nheader-value-here".to_owned()),
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
        BoundCredential::bearer(
            "Bearer",
            Redacted::new("x".repeat(MIN_CREDENTIAL_BYTES)),
            destinations(),
        )
        .expect("a minimum-length secret is a valid credential");
        let error = BoundCredential::bearer(
            "Bearer",
            Redacted::new("tell-nobody-not-even-once\n".to_owned()),
            destinations(),
        )
        .expect_err("control bytes are refused");
        assert!(!error.to_string().contains("tell-nobody"), "{error}");
    }

    #[test]
    fn drn_credential_identity_commits_the_complete_effective_scope() {
        let original = secret_grant(
            "api.example.test",
            SecretSinkKind::HttpBearer,
            None,
            "/v1/allowed",
        );
        let credential = BoundCredential::secret_bearer(
            SecretBytes::new(b"fixture-bearer-token".to_vec()),
            &original,
        )
        .expect("credential");
        let mut swapped = original;
        swapped.allowed_paths = vec![HttpPathRule::Exact {
            path: "/v1/other".to_owned(),
        }];
        let error = BufferedHttpClient::authorized_with_secret_credential(
            grant("api.example.test".to_owned(), "GET"),
            swapped,
            credential,
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect_err("same DRN and binding id cannot hide a swapped scope");
        assert!(matches!(
            error,
            ConfigurationError::InvalidCredential { .. }
        ));
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
        let plain_server = LoopbackServer::once(response);
        let plain_authority = plain_server.authority().to_owned();
        let server = LoopbackServer::once(response);
        let authority = server.authority().to_owned();

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

        let plain_request =
            String::from_utf8(plain_server.request()).expect("fixture request is UTF-8");
        assert!(
            !plain_request.to_ascii_lowercase().contains("authorization"),
            "{plain_request}"
        );
        let request = String::from_utf8(server.request()).expect("fixture request is UTF-8");
        assert!(
            request.contains("authorization: Bearer fixture-secret-value"),
            "{request}"
        );

        let plain_evidence = plain.into_evidence();
        let evidence = credentialed.into_evidence();
        assert!(!plain_evidence[0].credential_injected);
        assert!(evidence[0].credential_injected);
        let authority_delta = authority.len() as i64 - plain_authority.len() as i64;
        assert_eq!(
            evidence[0].request_bytes as i64 - authority_delta,
            plain_evidence[0].request_bytes as i64
        );

        plain_server.join();
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drn_bound_basic_auth_is_rendered_only_for_the_exact_path() {
        let response: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let server = LoopbackServer::once(response);
        let authority = server.authority().to_owned();
        let secret = secret_grant(
            &authority,
            SecretSinkKind::HttpBasic,
            Some("userA"),
            "/api/v1/thing",
        );
        let credential =
            BoundCredential::secret_basic(SecretBytes::new(b"fixture-password".to_vec()), &secret)
                .expect("valid Basic credential");
        let mut client = BufferedHttpClient::authorized_with_secret_credential(
            grant(authority.clone(), "GET"),
            secret,
            credential,
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid secret authorization");
        client
            .send(Request {
                method: "GET".to_owned(),
                uri: format!("http://{authority}/api/v1/thing"),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect("exact path succeeds");
        let request = String::from_utf8(server.request()).expect("request is UTF-8");
        assert!(
            request.contains("authorization: Basic dXNlckE6Zml4dHVyZS1wYXNzd29yZA=="),
            "{request}"
        );
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secret_scope_denies_path_prefix_confusion_and_queries_before_connection() {
        let secret = secret_grant(
            "127.0.0.1:9",
            SecretSinkKind::HttpBearer,
            None,
            "/api/v1/thing",
        );
        for uri in [
            "http://127.0.0.1:9/api/v1/things",
            "http://127.0.0.1:9/api/v1/thing?reflect=authorization",
            "http://127.0.0.1:9/api/v1/other",
            "http://127.0.0.1:9/api/v1/%74hing",
            "http://127.0.0.1:9\\api\\v1\\thing",
            "http://127.0.0.1:9/api/v1/x/../thing",
            "http://127.0.0.1:9/api//v1/thing",
        ] {
            let credential = BoundCredential::secret_bearer(
                SecretBytes::new(b"fixture-bearer-token".to_vec()),
                &secret,
            )
            .expect("credential");
            let mut client = BufferedHttpClient::authorized_with_secret_credential(
                grant("127.0.0.1:9".to_owned(), "GET"),
                secret.clone(),
                credential,
                HttpHostCeilings::default(),
                Duration::from_secs(1),
            )
            .expect("context");
            let error = client
                .send(Request {
                    method: "GET".to_owned(),
                    uri: uri.to_owned(),
                    headers: Vec::new(),
                    body: Vec::new(),
                })
                .await
                .expect_err("scope confusion is denied");
            assert_eq!(error.code, ErrorCode::Denied, "{uri}");
        }
    }

    #[test]
    fn a_drn_bound_bearer_secret_below_the_floor_is_refused() {
        let grant = secret_grant("api.example.test", SecretSinkKind::HttpBearer, None, "/v1");
        let error = BoundCredential::secret_bearer(
            SecretBytes::new(vec![b'x'; MIN_CREDENTIAL_BYTES - 1]),
            &grant,
        )
        .expect_err("a short resolved secret is refused");
        assert!(error.to_string().contains("16-byte minimum"), "{error}");
        assert!(!error.to_string().contains('x'), "{error}");
        BoundCredential::secret_bearer(SecretBytes::new(vec![b'x'; MIN_CREDENTIAL_BYTES]), &grant)
            .expect("a minimum-length resolved secret is a valid credential");
    }

    #[test]
    fn a_drn_bound_basic_secret_below_the_floor_is_refused() {
        let grant = secret_grant(
            "api.example.test",
            SecretSinkKind::HttpBasic,
            Some("userA"),
            "/v1",
        );
        let error = BoundCredential::secret_basic(
            SecretBytes::new(vec![b'x'; MIN_CREDENTIAL_BYTES - 1]),
            &grant,
        )
        .expect_err("a short resolved password is refused");
        assert!(error.to_string().contains("16-byte minimum"), "{error}");
        assert!(!error.to_string().contains('x'), "{error}");
        BoundCredential::secret_basic(SecretBytes::new(vec![b'x'; MIN_CREDENTIAL_BYTES]), &grant)
            .expect("a minimum-length resolved password is a valid credential");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_credential_echo_is_discarded() {
        let response: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nfixture-bearer-token";
        let server = LoopbackServer::once(response);
        let authority = server.authority().to_owned();
        let secret = secret_grant(&authority, SecretSinkKind::HttpBearer, None, "/echo");
        let credential = BoundCredential::secret_bearer(
            SecretBytes::new(b"fixture-bearer-token".to_vec()),
            &secret,
        )
        .expect("credential");
        let mut client = BufferedHttpClient::authorized_with_secret_credential(
            grant(authority.clone(), "GET"),
            secret,
            credential,
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("context");
        let error = client
            .send(Request {
                method: "GET".to_owned(),
                uri: format!("http://{authority}/echo"),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect_err("an echoed token is never returned");
        assert_eq!(error.code, ErrorCode::Denied);
        assert!(!error.message.contains("fixture-bearer-token"), "{error}");
        let evidence = client.into_evidence();
        assert_eq!(evidence[0].status, Some(200));
        assert!(evidence[0].response_bytes > 0);
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_credential_echo_is_discarded() {
        for body in ["fixture-secret-value", "Bearer fixture-secret-value"] {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let server = LoopbackServer::once(response.as_bytes());
            let authority = server.authority().to_owned();
            let mut client = BufferedHttpClient::authorized_with_credential(
                grant(authority.clone(), "GET"),
                Some(credential_for(&authority)),
                HttpHostCeilings::default(),
                Duration::from_secs(5),
            )
            .expect("valid fixture authorization");
            let error = client
                .send(Request {
                    method: "GET".to_owned(),
                    uri: format!("http://{authority}/echo"),
                    headers: Vec::new(),
                    body: Vec::new(),
                })
                .await
                .expect_err("an echoed legacy credential is never returned");
            assert_eq!(error.code, ErrorCode::Denied);
            assert!(!error.message.contains("fixture-secret-value"), "{error}");
            let evidence = client.into_evidence();
            assert_eq!(evidence[0].status, Some(200));
            assert!(evidence[0].credential_injected);
            server.join();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refuses_destinations_outside_the_credential_binding() {
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
        let evidence = client.into_evidence();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].authority, "127.0.0.1:9");
        assert_eq!(evidence[0].status, None);
        assert!(!evidence[0].credential_injected);
    }

    fn chatgpt_credential_for(authority: &str) -> BoundCredential {
        BoundCredential::chatgpt_subscription(
            Redacted::new("fixture-access-token".to_owned()),
            "acct-fixture",
            vec![authority.to_owned()],
        )
        .expect("valid fixture credential")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chatgpt_subscription_credential_injects_both_headers_outside_accounted_bytes() {
        let response: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let plain_server = LoopbackServer::once(response);
        let plain_authority = plain_server.authority().to_owned();
        let server = LoopbackServer::once(response);
        let authority = server.authority().to_owned();
        let request_to = |authority: &str| Request {
            method: "GET".to_owned(),
            uri: format!("http://{authority}/images/generations"),
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

        let mut client = BufferedHttpClient::authorized_with_credential(
            grant(authority.clone(), "GET"),
            Some(chatgpt_credential_for(&authority)),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        client
            .send(request_to(&authority))
            .await
            .expect("credentialed request succeeds");

        let request = String::from_utf8(server.request()).expect("fixture request is UTF-8");
        assert!(
            request.contains("authorization: Bearer fixture-access-token"),
            "{request}"
        );
        assert!(
            request.contains("chatgpt-account-id: acct-fixture"),
            "{request}"
        );

        let plain_evidence = plain.into_evidence();
        let evidence = client.into_evidence();
        assert!(evidence[0].credential_injected);
        let authority_delta = authority.len() as i64 - plain_authority.len() as i64;
        assert_eq!(
            evidence[0].request_bytes as i64 - authority_delta,
            plain_evidence[0].request_bytes as i64
        );
        let serialized = serde_json::to_string(&evidence).expect("evidence serializes");
        assert!(!serialized.contains("acct-fixture"), "{serialized}");
        assert!(!serialized.contains("fixture-access-token"), "{serialized}");
        assert!(!serialized.contains("chatgpt"), "{serialized}");

        plain_server.join();
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_guest_set_companion_header_is_rejected_not_overwritten() {
        let forged = || Request {
            method: "GET".to_owned(),
            uri: "http://127.0.0.1:9/".to_owned(),
            headers: vec![Header {
                name: "chatgpt-account-id".to_owned(),
                value: b"acct-guest-forged".to_vec(),
            }],
            body: Vec::new(),
        };

        let mut client = BufferedHttpClient::authorized_with_credential(
            grant("127.0.0.1:9".to_owned(), "GET"),
            Some(chatgpt_credential_for("127.0.0.1:9")),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = client
            .send(forged())
            .await
            .expect_err("a guest-set companion header must be rejected");
        assert_eq!(error.code, ErrorCode::InvalidHeader);

        let mut uncompanioned = BufferedHttpClient::authorized_with_credential(
            grant("127.0.0.1:9".to_owned(), "GET"),
            Some(credential_for("127.0.0.1:9")),
            HttpHostCeilings::default(),
            Duration::from_secs(1),
        )
        .expect("valid fixture authorization");
        let error = uncompanioned
            .send(forged())
            .await
            .expect_err("nothing listens on port 9");
        assert_ne!(error.code, ErrorCode::InvalidHeader);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_chatgpt_subscription_credential_refuses_destinations_outside_its_binding() {
        let mut client = BufferedHttpClient::authorized_with_credential(
            grant("127.0.0.1:9".to_owned(), "GET"),
            Some(chatgpt_credential_for("other.example.test")),
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
        let evidence = client.into_evidence();
        assert_eq!(evidence.len(), 1);
        assert!(!evidence[0].credential_injected);
    }

    #[test]
    fn a_chatgpt_subscription_credential_fails_closed_on_a_structural_account_id() {
        for account in ["", "acct with space", "acct\u{7f}control"] {
            let error = BoundCredential::chatgpt_subscription(
                Redacted::new("fixture-access-token".to_owned()),
                account,
                vec!["chatgpt.com".to_owned()],
            )
            .expect_err("a structurally invalid account identifier is refused");
            let rendered = error.to_string();
            assert!(rendered.contains("account identifier"), "{rendered}");
            assert!(!rendered.contains("fixture-access-token"), "{rendered}");
        }
        let error = BoundCredential::chatgpt_subscription(
            Redacted::new(String::new()),
            "acct-fixture",
            vec!["chatgpt.com".to_owned()],
        )
        .expect_err("an empty access token is refused");
        assert!(error.to_string().contains("16-byte minimum"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn guest_authorization_is_rejected_not_overwritten_when_a_credential_exists() {
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
        let server = LoopbackServer::once(oversized.as_bytes());
        let authority = server.authority().to_owned();
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
        server.join();

        let server = LoopbackServer::once(
            b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let authority = server.authority().to_owned();
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
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reaches_ipv6_literal_loopback_destinations() {
        let Some(server) = LoopbackServer::bound(
            "[::1]:0",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        ) else {
            return;
        };
        let authority = server.authority().to_owned();
        assert!(authority.starts_with("[::1]:"), "{authority}");

        let mut client = BufferedHttpClient::authorized(
            grant(authority.clone(), "GET"),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        let response = client
            .send(Request {
                method: "GET".to_owned(),
                uri: format!("http://{authority}/items"),
                headers: Vec::new(),
                body: Vec::new(),
            })
            .await
            .expect("an IPv6 literal loopback destination is reachable");

        assert_eq!(response.status, 200);
        let request = server.request();
        assert!(request.starts_with(b"GET /items HTTP/1.1\r\n"));
        let evidence = client.into_evidence();
        assert_eq!(evidence[0].authority, authority);
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reuses_one_pinned_client_across_calls_to_the_same_authority() {
        // Client-level pinning is deterministic, but whether hyper's connection pool reuses one TCP
        // connection is a background-task race this crate doesn't control, so the fixture tolerates
        // it without requiring it.
        let server = LoopbackServer::pooled(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok", 2);
        let authority = server.authority().to_owned();
        let mut client = BufferedHttpClient::authorized(
            grant(authority.clone(), "GET"),
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        for _ in 0..2 {
            let response = client
                .send(Request {
                    method: "GET".to_owned(),
                    uri: format!("http://{authority}/items"),
                    headers: Vec::new(),
                    body: Vec::new(),
                })
                .await
                .expect("both calls share one pooled connection");
            assert_eq!(response.status, 200);
        }
        assert_eq!(client.into_evidence().len(), 2);
        server.join();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_pin_set_builds_a_new_client() {
        let response: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        let first_server = LoopbackServer::once(response);
        let first = first_server.authority().to_owned();
        let second_server = LoopbackServer::once(response);
        let second = second_server.authority().to_owned();

        let mut grant = grant(first.clone(), "GET");
        grant.allowed_hosts.push(second.clone());
        let mut client = BufferedHttpClient::authorized(
            grant,
            HttpHostCeilings::default(),
            Duration::from_secs(5),
        )
        .expect("valid fixture authorization");
        for (authority, path) in [(&first, "/first"), (&second, "/second")] {
            let response = client
                .send(Request {
                    method: "GET".to_owned(),
                    uri: format!("http://{authority}{path}"),
                    headers: Vec::new(),
                    body: Vec::new(),
                })
                .await
                .expect("each destination is reachable");
            assert_eq!(response.status, 200);
        }

        assert!(
            first_server
                .request()
                .starts_with(b"GET /first HTTP/1.1\r\n")
        );
        assert!(
            second_server
                .request()
                .starts_with(b"GET /second HTTP/1.1\r\n")
        );
        let evidence = client.into_evidence();
        assert_eq!(evidence[0].authority, first);
        assert_eq!(evidence[1].authority, second);
        first_server.join();
        second_server.join();
    }

    #[test]
    fn bounds_and_sanitizes_error_messages() {
        let message = bounded_message(&format!("line one\n{}", "x".repeat(400)));
        assert!(message.len() <= 256);
        assert!(!message.contains('\n'));
    }
}
