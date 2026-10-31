use parking_lot::Mutex;
use std::{
    env,
    ffi::OsString,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dekopon_core::{
    Redacted,
    private_file::{self, PrivateFileLock, TemporarySweep},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    sync::{Semaphore, oneshot},
    task::JoinSet,
};
use tracing::{Instrument as _, instrument::WithSubscriber as _};

use crate::http::InferenceHttp;
use crate::model::{MAX_ERROR_BODY_BYTES, sanitize_diagnostic};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const LOGIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(10);
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REFRESH_MARGIN: Duration = Duration::from_secs(60);
const AUTH_VERSION: u32 = 1;
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatGptAuthStatus {
    pub path: PathBuf,
    pub signed_in: bool,
    pub expired: bool,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
    Adopted,
    Rotated,
    RotatedUnsaved,
}

impl RefreshOutcome {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Adopted => "adopted",
            Self::Rotated => "rotated",
            Self::RotatedUnsaved => "rotated-unsaved",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedCredential {
    pub access: Redacted<String>,
    pub account_id: String,
    pub refresh: Option<RefreshOutcome>,
}

#[derive(Clone)]
pub struct RefreshClient {
    pub(crate) http: InferenceHttp,
    timeout: Duration,
}

impl RefreshClient {
    pub fn new(
        timeout: Duration,
        extra_ca_bundles: &[Redacted<String>],
    ) -> Result<Self, crate::error::InferenceError> {
        Ok(Self {
            http: InferenceHttp::new(timeout, extra_ca_bundles)?,
            timeout,
        })
    }
}

pub struct CredentialFile {
    admission: Arc<Semaphore>,
    tasks: Mutex<JoinSet<()>>,
    path: PathBuf,
    credentials: Mutex<ChatGptCredentials>,
    endpoints: ChatGptEndpoints,
}

impl fmt::Debug for CredentialFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Debug on CredentialFile must render only the path field, since the struct also holds a
        // live access token and refresh token.
        formatter
            .debug_struct("CredentialFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl CredentialFile {
    pub fn open(auth_path: &Path) -> Result<Self, ChatGptError> {
        Self::with_endpoints(auth_path, ChatGptEndpoints::production())
    }

    pub(crate) fn with_endpoints(
        auth_path: &Path,
        endpoints: ChatGptEndpoints,
    ) -> Result<Self, ChatGptError> {
        let credentials = load_credentials(auth_path)?;
        Ok(Self {
            admission: Arc::new(Semaphore::new(1)),
            tasks: Mutex::new(JoinSet::new()),
            path: auth_path.to_path_buf(),
            credentials: Mutex::new(credentials),
            endpoints,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn status(&self) -> Result<ChatGptAuthStatus, ChatGptError> {
        let credentials = self.snapshot();
        Ok(ChatGptAuthStatus {
            path: self.path.clone(),
            signed_in: true,
            expired: credentials.expires_at <= unix_time()?,
            expires_at: Some(credentials.expires_at),
        })
    }

    pub async fn current(
        self: &Arc<Self>,
        client: &RefreshClient,
    ) -> Result<ResolvedCredential, ChatGptError> {
        self.refresh_if_needed(client, None).await
    }

    pub(crate) fn unrefreshed(&self) -> Result<ResolvedCredential, ChatGptError> {
        let credentials = self.snapshot();
        if needs_refresh(&credentials)? {
            return Err(ChatGptError::Configuration(
                "credential needs a refresh, which a loopback endpoint never performs".to_owned(),
            ));
        }
        Ok(ResolvedCredential {
            access: credentials.access,
            account_id: credentials.account_id,
            refresh: None,
        })
    }

    pub async fn force_refresh(
        self: &Arc<Self>,
        client: &RefreshClient,
        rejected: &Redacted<String>,
    ) -> Result<ResolvedCredential, ChatGptError> {
        self.refresh_if_needed(client, Some(rejected.clone())).await
    }

    pub async fn drain(&self) {
        let mut tasks = {
            let mut tasks = self.tasks.lock();
            self.admission.close();
            std::mem::take(&mut *tasks)
        };
        while let Some(result) = tasks.join_next().await {
            observe_refresh_task(result);
        }
    }

    fn snapshot(&self) -> ChatGptCredentials {
        self.credentials.lock().clone()
    }

    fn install(&self, credentials: &ChatGptCredentials) {
        let mut stored = self.credentials.lock();
        *stored = credentials.clone();
    }

    async fn refresh_if_needed(
        self: &Arc<Self>,
        client: &RefreshClient,
        rejected: Option<Redacted<String>>,
    ) -> Result<ResolvedCredential, ChatGptError> {
        let credentials = self.snapshot();
        if rejected.is_none() && !needs_refresh(&credentials)? {
            return Ok(resolved(credentials, None));
        }
        let permit = Arc::clone(&self.admission)
            .acquire_owned()
            .await
            .map_err(|source| ChatGptError::RefreshTask(io::Error::other(source)))?;
        let permit = Arc::new(permit);
        let lock = tokio::time::timeout(client.timeout, self.acquire_lock(&permit))
            .await
            .map_err(|source| {
                ChatGptError::RefreshTask(io::Error::new(io::ErrorKind::TimedOut, source))
            })??;
        let (reply, wait) = oneshot::channel();
        {
            let mut tasks = self.tasks.lock();
            if self.admission.is_closed() {
                return Err(ChatGptError::RefreshTask(io::Error::other(
                    "credential refresh owner shut down",
                )));
            }
            while let Some(result) = tasks.try_join_next() {
                observe_refresh_task(result);
            }
            let owner = Arc::clone(self);
            let client = client.clone();
            let span = tracing::Span::current();
            tasks.spawn(
                async move {
                    let _permit = permit;
                    let result = owner.refresh_locked(lock, client, rejected).await;
                    let _waiter_dropped = reply.send(result);
                }
                .instrument(span)
                .with_current_subscriber(),
            );
        }
        wait.await
            .map_err(|source| ChatGptError::RefreshTask(io::Error::other(source)))?
    }

    async fn acquire_lock(
        &self,
        permit: &Arc<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<PrivateFileLock, ChatGptError> {
        loop {
            if self.admission.is_closed() {
                return Err(ChatGptError::RefreshTask(io::Error::other(
                    "credential refresh owner shut down",
                )));
            }
            let path = self.path.clone();
            let permit = Arc::clone(permit);
            let attempt = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                PrivateFileLock::try_acquire(&path)
            })
            .await
            .map_err(|source| ChatGptError::RefreshTask(io::Error::other(source)))?
            .map_err(|error| ChatGptError::LockAuth {
                path: error.path,
                source: error.source,
            })?;
            if let Some(lock) = attempt {
                return Ok(lock);
            }
            tokio::time::sleep(LOCK_POLL).await;
        }
    }

    async fn refresh_locked(
        self: &Arc<Self>,
        lock: PrivateFileLock,
        client: RefreshClient,
        rejected: Option<Redacted<String>>,
    ) -> Result<ResolvedCredential, ChatGptError> {
        let span = tracing::info_span!(
            "chatgpt.refresh",
            forced = rejected.is_some(),
            outcome = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            credential.expires_at = tracing::field::Empty,
        );
        let started = Instant::now();
        let result = async {
            let _lock = lock;
            let owner = Arc::clone(self);
            let (mut credentials, adopted) = tokio::task::spawn_blocking(move || {
                owner.adopt(rejected.as_ref())
            }).await.map_err(|source| ChatGptError::RefreshTask(io::Error::other(source)))??;
            if adopted {
                return Ok(resolved(credentials, Some(RefreshOutcome::Adopted)));
            }
            credentials = refresh_credentials(&client.http, client.timeout, &self.endpoints, &credentials).await?;
            let owner = Arc::clone(self);
            let save_span = tracing::Span::current();
            let save_dispatch = tracing::dispatcher::get_default(Clone::clone);
            let (credentials, outcome) = tokio::task::spawn_blocking(move || tracing::dispatcher::with_default(&save_dispatch, || save_span.in_scope(|| {
                let outcome = match save_credentials(&owner.path, &credentials) {
                    Ok(()) => RefreshOutcome::Rotated,
                    Err(error) => {
                        tracing::error!(event = "chatgpt_credential_save_failed", path = %owner.path.display(), error = %error, "chatgpt.credential.save.failed");
                        RefreshOutcome::RotatedUnsaved
                    }
                };
                owner.install(&credentials);
                (credentials, outcome)
            }))).await.map_err(|source| ChatGptError::RefreshTask(io::Error::other(source)))?;
            Ok(resolved(credentials, Some(outcome)))
        }.instrument(span.clone()).await;
        match &result {
            Ok(resolved) => record_refresh(
                &span,
                resolved.refresh.map_or("current", RefreshOutcome::label),
                started,
                self.snapshot().expires_at,
            ),
            Err(_) => {
                span.record("outcome", "failed");
                span.record("otel.status_code", "error");
                span.record("duration_ms", elapsed_ms(started));
            }
        }
        result
    }

    fn adopt(
        &self,
        rejected: Option<&Redacted<String>>,
    ) -> Result<(ChatGptCredentials, bool), ChatGptError> {
        let mut credentials = self.snapshot();
        let installed_replacement = rejected
            .is_some_and(|token| credentials.access.expose() != token.expose())
            && !needs_refresh(&credentials)?;
        let adopted =
            !installed_replacement && adopt_stored_credentials(&self.path, &mut credentials);
        if let Some(rejected) = rejected
            && credentials.access.expose() == rejected.expose()
            && let Ok(stored) = load_credentials(&self.path)
            && stored.access.expose() != rejected.expose()
            && !needs_refresh(&stored)?
        {
            credentials = stored;
        }
        let replaced = rejected.is_none_or(|token| credentials.access.expose() != token.expose());
        let adopted = (adopted || replaced) && !needs_refresh(&credentials)?;
        if adopted {
            self.install(&credentials);
        }
        Ok((credentials, adopted))
    }
}

fn resolved(
    credentials: ChatGptCredentials,
    refresh: Option<RefreshOutcome>,
) -> ResolvedCredential {
    ResolvedCredential {
        access: credentials.access,
        account_id: credentials.account_id,
        refresh,
    }
}

fn observe_refresh_task(result: Result<(), tokio::task::JoinError>) {
    if let Err(source) = result {
        tracing::error!(error = %source, "ChatGPT refresh task failed");
    }
}

pub async fn login(auth_path: Option<&Path>) -> Result<PathBuf, ChatGptError> {
    login_on_stderr(auth_path, ChatGptEndpoints::production()).await
}

async fn login_on_stderr(
    auth_path: Option<&Path>,
    endpoints: ChatGptEndpoints,
) -> Result<PathBuf, ChatGptError> {
    let mut output = io::stderr();
    login_with_endpoints(auth_path, endpoints, &mut output).await
}

async fn login_with_endpoints(
    auth_path: Option<&Path>,
    endpoints: ChatGptEndpoints,
    output: &mut dyn Write,
) -> Result<PathBuf, ChatGptError> {
    let path = resolve_auth_path(auth_path)?;
    let http = InferenceHttp::new(LOGIN_REQUEST_TIMEOUT, &[])
        .map_err(|source| ChatGptError::Configuration(source.to_string()))?;
    let device = start_device_login(&http, &endpoints).await?;
    writeln!(output, "Open {}", endpoints.verification_url)
        .and_then(|()| writeln!(output, "Enter code: {}", device.user_code))
        .and_then(|()| writeln!(output, "Waiting for ChatGPT authorization…"))
        .map_err(|source| ChatGptError::Output { source })?;
    output
        .flush()
        .map_err(|source| ChatGptError::Output { source })?;

    let authorization = poll_device_login(&http, &endpoints, &device).await?;
    let credentials = exchange_authorization(&http, &endpoints, &authorization).await?;
    let saved_path = path.clone();
    tokio::task::spawn_blocking(move || save_credentials(&saved_path, &credentials))
        .await
        .map_err(|source| ChatGptError::RefreshTask(io::Error::other(source)))??;
    Ok(path)
}

pub fn status(auth_path: Option<&Path>) -> Result<ChatGptAuthStatus, ChatGptError> {
    let path = resolve_auth_path(auth_path)?;
    let credentials = match load_credentials(&path) {
        Ok(credentials) => credentials,
        Err(ChatGptError::NotLoggedIn { .. }) => {
            return Ok(ChatGptAuthStatus {
                path,
                signed_in: false,
                expired: false,
                expires_at: None,
            });
        }
        Err(error) => return Err(error),
    };
    Ok(ChatGptAuthStatus {
        path,
        signed_in: true,
        expired: credentials.expires_at <= unix_time()?,
        expires_at: Some(credentials.expires_at),
    })
}

pub fn logout(auth_path: Option<&Path>) -> Result<PathBuf, ChatGptError> {
    let path = resolve_auth_path(auth_path)?;
    sweep_stale_temporaries(&path, None);
    match fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path),
        Err(source) => Err(ChatGptError::RemoveAuth {
            path: path.clone(),
            source,
        }),
    }
}

#[derive(Debug)]
pub struct ChatGptCredentialExport {
    path: PathBuf,
    document: Redacted<String>,
}

impl ChatGptCredentialExport {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn expose_document(&self) -> &str {
        self.document.expose()
    }
}

pub fn export_credentials(
    auth_path: Option<&Path>,
) -> Result<ChatGptCredentialExport, ChatGptError> {
    let path = resolve_auth_path(auth_path)?;
    let credentials = load_credentials(&path)?;
    let document = serde_json::to_string(&credentials)
        .map(|json| format!("{json}\n"))
        .map_err(|source| ChatGptError::SerializeAuth {
            path: path.clone(),
            source,
        })?;

    Ok(ChatGptCredentialExport {
        path,
        document: Redacted::new(document),
    })
}

#[derive(Clone)]
pub(crate) struct ChatGptEndpoints {
    device_code: String,
    device_token: String,
    token: String,
    verification_url: String,
    #[cfg(test)]
    pub(crate) responses: String,
}

impl ChatGptEndpoints {
    fn production() -> Self {
        Self {
            device_code: format!("{AUTH_BASE_URL}/api/accounts/deviceauth/usercode"),
            device_token: format!("{AUTH_BASE_URL}/api/accounts/deviceauth/token"),
            token: format!("{AUTH_BASE_URL}/oauth/token"),
            verification_url: format!("{AUTH_BASE_URL}/codex/device"),
            #[cfg(test)]
            responses: crate::codex::RESPONSES_URL.to_owned(),
        }
    }

    #[cfg(test)]
    pub(crate) fn local(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            device_code: format!("{base}/device-code"),
            device_token: format!("{base}/device-token"),
            token: format!("{base}/token"),
            verification_url: format!("{base}/verify"),
            responses: format!("{base}/responses"),
        }
    }
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: Value,
}

struct DeviceLogin {
    device_auth_id: String,
    user_code: String,
    interval: Duration,
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    authorization_code: String,
    code_verifier: String,
}

struct DeviceAuthorization {
    code: Redacted<String>,
    verifier: Redacted<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Redacted<String>,
    refresh_token: Redacted<String>,
    expires_in: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatGptCredentials {
    version: u32,
    // These fields are exposed in the clear only for this struct's serialization; any other struct
    // they end up in still redacts them by default.
    #[serde(serialize_with = "dekopon_core::serialize_exposed")]
    access: Redacted<String>,
    #[serde(serialize_with = "dekopon_core::serialize_exposed")]
    refresh: Redacted<String>,
    expires_at: u64,
    account_id: String,
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
async fn start_device_login(
    http: &InferenceHttp,
    endpoints: &ChatGptEndpoints,
) -> Result<DeviceLogin, ChatGptError> {
    let response = http
        .post(&endpoints.device_code)
        .timeout(LOGIN_REQUEST_TIMEOUT)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(json!({"client_id": CLIENT_ID}).to_string())
        .send()
        .await
        .map_err(|error| ChatGptError::Request(error.to_string()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = oauth_failure_detail(response).await;
        return Err(ChatGptError::Login(format!(
            "device authorization returned HTTP {status}: {detail}"
        )));
    }
    let response: DeviceCodeResponse =
        read_json(response, crate::diagnostic::DiagnosticSecrets::default()).await?;
    if response.device_auth_id.trim().is_empty() || response.user_code.trim().is_empty() {
        return Err(ChatGptError::Protocol(
            "device authorization response omitted required fields".to_owned(),
        ));
    }
    let interval = match response.interval {
        Value::Number(number) => number.as_f64(),
        Value::String(string) => string.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
    .ok_or_else(|| ChatGptError::Protocol("invalid device polling interval".to_owned()))?;

    Ok(DeviceLogin {
        device_auth_id: response.device_auth_id,
        user_code: response.user_code,
        interval: Duration::from_secs_f64(interval.clamp(1.0, 30.0)),
    })
}

async fn poll_device_login(
    http: &InferenceHttp,
    endpoints: &ChatGptEndpoints,
    device: &DeviceLogin,
) -> Result<DeviceAuthorization, ChatGptError> {
    let started = Instant::now();
    let mut interval = device.interval;
    let mut transport_failure: Option<String> = None;
    while started.elapsed() < DEVICE_LOGIN_TIMEOUT {
        let remaining = DEVICE_LOGIN_TIMEOUT.saturating_sub(started.elapsed());
        tokio::time::sleep(interval.min(remaining)).await;
        let response = match http
            .post(&endpoints.device_token)
            .timeout(LOGIN_REQUEST_TIMEOUT)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(
                json!({
                    "device_auth_id": device.device_auth_id,
                    "user_code": device.user_code,
                })
                .to_string(),
            )
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    event = "chatgpt_device_login_poll_failed",
                    error = %error,
                    "chatgpt.device.login.poll.failed"
                );
                transport_failure = Some(error.to_string());
                interval = backed_off(interval);
                continue;
            }
        };
        transport_failure = None;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            let response: DeviceAuthorizationResponse =
                read_json(response, crate::diagnostic::DiagnosticSecrets::default()).await?;
            return Ok(DeviceAuthorization {
                code: Redacted::new(response.authorization_code),
                verifier: Redacted::new(response.code_verifier),
            });
        }
        let body = read_error_body(response, crate::diagnostic::DiagnosticSecrets::default()).await;
        let error_code = oauth_error_code(&body);
        if status == 403
            || status == 404
            || error_code.as_deref() == Some("deviceauth_authorization_pending")
        {
            continue;
        }
        if error_code.as_deref() == Some("slow_down") || status == 429 {
            interval = backed_off(interval);
            continue;
        }
        return Err(ChatGptError::Login(format!(
            "device authorization failed with HTTP {status}: {}",
            error_code.unwrap_or(body)
        )));
    }
    match transport_failure {
        Some(error) => Err(ChatGptError::Request(error)),
        None => Err(ChatGptError::LoginTimeout),
    }
}

fn backed_off(interval: Duration) -> Duration {
    interval
        .saturating_add(Duration::from_secs(5))
        .min(Duration::from_secs(30))
}

async fn exchange_authorization(
    http: &InferenceHttp,
    endpoints: &ChatGptEndpoints,
    authorization: &DeviceAuthorization,
) -> Result<ChatGptCredentials, ChatGptError> {
    request_token(
        http,
        LOGIN_REQUEST_TIMEOUT,
        &endpoints.token,
        [
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", authorization.code.expose().as_str()),
            ("code_verifier", authorization.verifier.expose().as_str()),
            ("redirect_uri", DEVICE_REDIRECT_URI),
        ],
        crate::diagnostic::DiagnosticSecrets::new(Some(&authorization.code))
            .with_previous(&authorization.verifier),
    )
    .await
}

async fn refresh_credentials(
    http: &InferenceHttp,
    timeout: Duration,
    endpoints: &ChatGptEndpoints,
    credentials: &ChatGptCredentials,
) -> Result<ChatGptCredentials, ChatGptError> {
    request_token(
        http,
        timeout,
        &endpoints.token,
        [
            ("grant_type", "refresh_token"),
            ("refresh_token", credentials.refresh.expose().as_str()),
            ("client_id", CLIENT_ID),
        ],
        crate::diagnostic::DiagnosticSecrets::new(Some(&credentials.refresh))
            .with_previous(&credentials.access),
    )
    .await
}

async fn request_token<'a, const N: usize>(
    http: &InferenceHttp,
    timeout: Duration,
    endpoint: &str,
    form: [(&'a str, &'a str); N],
    secrets: crate::diagnostic::DiagnosticSecrets<'_>,
) -> Result<ChatGptCredentials, ChatGptError> {
    let response = http
        .post(endpoint)
        .timeout(timeout)
        .form(form.as_slice())
        .send()
        .await
        .map_err(|error| ChatGptError::Request(secrets.sanitize(&error.to_string())))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = read_error_body(response, secrets).await;
        let code = oauth_error_code(&body).map(|value| secrets.sanitize(&value));
        return Err(ChatGptError::TokenRefused {
            status,
            detail: secrets.sanitize(&oauth_detail(&body, code.as_deref())),
            code,
        });
    }
    let token: TokenResponse = read_json(response, secrets).await?;
    if token.access_token.expose().is_empty() || token.refresh_token.expose().is_empty() {
        return Err(ChatGptError::Protocol(
            "token response omitted required credentials".to_owned(),
        ));
    }
    let account_id = extract_account_id(token.access_token.expose())?;
    Ok(ChatGptCredentials {
        version: AUTH_VERSION,
        access: token.access_token,
        refresh: token.refresh_token,
        expires_at: unix_time()?.saturating_add(token.expires_in),
        account_id,
    })
}

fn extract_account_id(access: &str) -> Result<String, ChatGptError> {
    let payload = access
        .split('.')
        .nth(1)
        .ok_or_else(|| ChatGptError::Protocol("access token is not a JWT".to_owned()))?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).map_err(|source| {
        ChatGptError::Protocol(format!("access token has invalid JWT encoding: {source}"))
    })?;
    let payload = serde_json::from_slice::<Value>(&bytes).map_err(|source| {
        ChatGptError::Protocol(format!("access token has invalid JWT JSON: {source}"))
    })?;
    payload
        .get(JWT_AUTH_CLAIM)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ChatGptError::Protocol("access token omitted ChatGPT account ID".to_owned()))
}

async fn read_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    secrets: crate::diagnostic::DiagnosticSecrets<'_>,
) -> Result<T, ChatGptError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| ChatGptError::Protocol(secrets.sanitize(&source.to_string())))?
    {
        if bytes.len().saturating_add(chunk.len()) > crate::http::MAX_BUFFERED_BYTES {
            return Err(ChatGptError::Protocol(
                "authentication response is too large".to_owned(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|source| ChatGptError::Protocol(secrets.sanitize(&source.to_string())))
}

async fn read_error_body(
    mut response: reqwest::Response,
    secrets: crate::diagnostic::DiagnosticSecrets<'_>,
) -> String {
    let mut bytes = Vec::new();
    let truncated = loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = (MAX_ERROR_BODY_BYTES as usize + 1).saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if bytes.len() > MAX_ERROR_BODY_BYTES as usize {
                    break true;
                }
            }
            Ok(None) => break false,
            Err(_source) => break true,
        }
    };
    let text = if truncated {
        secrets.sanitize_truncated(&bytes)
    } else {
        secrets.sanitize(&String::from_utf8_lossy(&bytes))
    };
    if text.trim().is_empty() {
        return "no response body".to_owned();
    }
    text
}

async fn oauth_failure_detail(response: reqwest::Response) -> String {
    let body = read_error_body(response, crate::diagnostic::DiagnosticSecrets::default()).await;
    let code = oauth_error_code(&body);
    oauth_detail(&body, code.as_deref())
}

fn oauth_detail(body: &str, code: Option<&str>) -> String {
    let Some(code) = code else {
        return body.to_owned();
    };
    match oauth_error_description(body) {
        Some(description) => format!("{code}: {description}"),
        None => code.to_owned(),
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
fn oauth_error_code(body: &str) -> Option<String> {
    match serde_json::from_str::<Value>(body).ok()?.get("error")? {
        Value::String(code) => Some(code.clone()),
        Value::Object(object) => object
            .get("code")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn oauth_error_description(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .get("error_description")
        .and_then(Value::as_str)
        .filter(|description| !description.trim().is_empty())
        .map(sanitize_diagnostic)
}

pub const DEFAULT_AUTH_FILE_NAME: &str = "chatgpt-auth.json";

pub fn resolve_auth_path_named(
    explicit: Option<&Path>,
    file_name: &str,
) -> Result<PathBuf, ChatGptError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    AuthPathEnvironment::from_process().resolve(file_name)
}

struct AuthPathEnvironment {
    environment: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    app_data: Option<PathBuf>,
}

impl AuthPathEnvironment {
    fn from_process() -> Self {
        Self {
            environment: exported_path(env::var_os("DEKOPON_CHATGPT_AUTH_FILE")),
            xdg_config_home: exported_path(env::var_os("XDG_CONFIG_HOME")),
            home: exported_path(env::var_os("HOME")),
            app_data: exported_path(env::var_os("APPDATA")),
        }
    }

    fn resolve(&self, file_name: &str) -> Result<PathBuf, ChatGptError> {
        let resolved = if let Some(path) = &self.environment {
            path.clone()
        } else if let Some(config) = &self.xdg_config_home {
            config.join("dekopon").join(file_name)
        } else if let Some(home) = &self.home {
            home.join(".config").join("dekopon").join(file_name)
        } else if let Some(app_data) = &self.app_data {
            app_data.join("dekopon").join(file_name)
        } else {
            return Err(ChatGptError::Configuration(
                "could not determine credential path; set DEKOPON_CHATGPT_AUTH_FILE".to_owned(),
            ));
        };
        if resolved.is_relative() {
            return Err(ChatGptError::Configuration(format!(
                "credential path {} is relative; set DEKOPON_CHATGPT_AUTH_FILE to an absolute path",
                resolved.display()
            )));
        }
        Ok(resolved)
    }
}

fn exported_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

pub fn resolve_auth_path(explicit: Option<&Path>) -> Result<PathBuf, ChatGptError> {
    resolve_auth_path_named(explicit, DEFAULT_AUTH_FILE_NAME)
}

fn needs_refresh(credentials: &ChatGptCredentials) -> Result<bool, ChatGptError> {
    let refresh_at = credentials
        .expires_at
        .saturating_sub(REFRESH_MARGIN.as_secs());
    Ok(unix_time()? >= refresh_at)
}

fn adopt_stored_credentials(path: &Path, credentials: &mut ChatGptCredentials) -> bool {
    let Ok(stored) = load_credentials(path) else {
        return false;
    };
    if stored.expires_at <= credentials.expires_at {
        return false;
    }
    *credentials = stored;
    true
}

fn record_refresh(span: &tracing::Span, outcome: &str, started: Instant, expires_at: u64) {
    span.record("outcome", outcome);
    span.record("duration_ms", elapsed_ms(started));
    span.record("credential.expires_at", expires_at);
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
fn credential_lock_path(auth_path: &Path) -> Option<PathBuf> {
    private_file::lock_path(auth_path)
}

fn load_credentials(path: &Path) -> Result<ChatGptCredentials, ChatGptError> {
    let bytes = fs::read(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ChatGptError::NotLoggedIn {
                path: path.to_path_buf(),
            }
        } else {
            ChatGptError::ReadAuth {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let credentials = serde_json::from_slice::<ChatGptCredentials>(&bytes).map_err(|source| {
        ChatGptError::ParseAuth {
            path: path.to_path_buf(),
            category: source.classify(),
            line: source.line(),
            column: source.column(),
        }
    })?;
    if credentials.version != AUTH_VERSION {
        return Err(ChatGptError::Configuration(format!(
            "ChatGPT credentials at {} have an unsupported `version`; expected {AUTH_VERSION}",
            path.display()
        )));
    }
    if credentials.access.expose().is_empty()
        || credentials.refresh.expose().is_empty()
        || credentials.account_id.is_empty()
    {
        return Err(ChatGptError::Configuration(format!(
            "ChatGPT credentials at {} are incomplete; `access`, `refresh` and `accountId` must be non-empty",
            path.display()
        )));
    }
    Ok(credentials)
}

fn save_credentials(path: &Path, credentials: &ChatGptCredentials) -> Result<(), ChatGptError> {
    let parent = path.parent().ok_or_else(|| {
        ChatGptError::Configuration("credential path must have a parent directory".to_owned())
    })?;
    fs::create_dir_all(parent).map_err(|source| ChatGptError::WriteAuth {
        path: path.to_path_buf(),
        source,
    })?;

    let temporary = private_file::temporary_path(path);
    sweep_stale_temporaries(path, Some(&temporary));
    let mut bytes =
        serde_json::to_vec(credentials).map_err(|source| ChatGptError::SerializeAuth {
            path: temporary,
            source,
        })?;
    bytes.push(b'\n');
    private_file::replace_private_file(path, &bytes).map_err(|error| ChatGptError::WriteAuth {
        path: error.path,
        source: error.source,
    })
}

// Sweeping other writers' plaintext is safe because refreshes lock and logins are human-paced.
fn sweep_stale_temporaries(path: &Path, keep: Option<&Path>) {
    for outcome in private_file::sweep_stale_temporaries(path, keep) {
        match outcome {
            TemporarySweep::Failed { path, source } => {
                tracing::warn!(
                    event = "chatgpt_credential_temporary_orphaned",
                    path = %path.display(),
                    error = %source,
                    "chatgpt.credential.temporary.orphaned"
                );
            }
            TemporarySweep::Removed { path } => {
                tracing::warn!(
                    event = "chatgpt_credential_temporary_swept",
                    path = %path.display(),
                    "chatgpt.credential.temporary.swept"
                );
            }
        }
    }
}

#[allow(
    clippy::map_err_ignore,
    reason = "SystemTimeError carries only how far the clock sits before the epoch, which is the \
              same fact the message already states; the operator's fix is to set the clock either \
              way"
)]
fn unix_time() -> Result<u64, ChatGptError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| ChatGptError::Configuration("system clock is before Unix epoch".to_owned()))
}

const fn parse_problem(category: serde_json::error::Category) -> &'static str {
    match category {
        serde_json::error::Category::Io
        | serde_json::error::Category::Syntax
        | serde_json::error::Category::Eof => "the file is not valid JSON",
        serde_json::error::Category::Data => {
            "expected an object with integer `version` and `expiresAt` and string `access`, `refresh` and `accountId`"
        }
    }
}

#[derive(Debug, Error)]
pub enum ChatGptError {
    #[error("invalid ChatGPT configuration: {0}")]
    Configuration(String),
    #[error("not logged in to ChatGPT; run `dekopon-gatewayd auth chatgpt login` (expected {})", path.display())]
    NotLoggedIn { path: PathBuf },
    #[error("could not read ChatGPT credentials at {}", path.display())]
    ReadAuth {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    // Serde's messages quote the mismatched value, which may be token bytes, so only its
    // category and position are kept.
    #[error("could not parse ChatGPT credentials at {}: {} at line {line}, column {column}", path.display(), parse_problem(*category))]
    ParseAuth {
        path: PathBuf,
        category: serde_json::error::Category,
        line: usize,
        column: usize,
    },
    #[error("could not serialize ChatGPT credentials at {}", path.display())]
    SerializeAuth {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not lock ChatGPT credential refresh at {}", path.display())]
    LockAuth {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write ChatGPT credentials at {}", path.display())]
    WriteAuth {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not remove ChatGPT credentials at {}", path.display())]
    RemoveAuth {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write ChatGPT login instructions")]
    Output {
        #[source]
        source: io::Error,
    },
    #[error("ChatGPT credential refresh task failed")]
    RefreshTask(#[source] io::Error),
    #[error("ChatGPT authentication request failed: {0}")]
    Request(String),
    #[error("invalid ChatGPT authentication response: {0}")]
    Protocol(String),
    #[error("ChatGPT login failed: {0}")]
    Login(String),
    #[error("ChatGPT token endpoint returned HTTP {status}: {detail}")]
    TokenRefused {
        status: u16,
        code: Option<String>,
        detail: String,
    },
    #[error("ChatGPT device login timed out after 15 minutes")]
    LoginTimeout,
}

#[cfg(test)]
mod tests {
    use std::{fs, io, path::Path, sync::Arc, time::Duration};

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tracing_subscriber::util::SubscriberInitExt as _;

    use crate::codex::{CodexClient, replay_transcript, request_body_json};
    use crate::{
        control::TurnControl,
        inference::{GenerateRequest, InferenceModel},
    };
    use dekopon_core::Redacted;
    use std::ops::ControlFlow;

    use super::{
        AUTH_VERSION, AuthPathEnvironment, ChatGptCredentials, ChatGptEndpoints, ChatGptError,
        CredentialFile, DEFAULT_AUTH_FILE_NAME, OsString, PathBuf, PrivateFileLock, RefreshClient,
        RefreshOutcome, credential_lock_path, export_credentials, exported_path,
        extract_account_id, load_credentials, login_with_endpoints, logout, save_credentials,
        status,
    };
    use crate::{
        mock::{MockResponse, MockServer},
        model::{CompletionOptions, ContentPart, ModelMessage, ModelTool},
        stream::TurnEvent,
    };

    async fn generate_turn(
        model: &CodexClient,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<crate::model::AssistantTurn, crate::error::InferenceError> {
        let control =
            TurnControl::new(tokio::sync::watch::channel(false).1, Duration::from_secs(2))?;
        model
            .generate(
                GenerateRequest {
                    messages,
                    tools,
                    options,
                },
                observe,
                &control,
            )
            .await
    }

    fn ignored(_event: TurnEvent) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }

    fn recorded(events: &[TurnEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                TurnEvent::TextDelta(text) => format!("text:{}", text.as_str()),
            })
            .collect()
    }

    fn exports(
        environment: Option<&str>,
        xdg_config_home: Option<&str>,
        home: Option<&str>,
    ) -> AuthPathEnvironment {
        let export = |value: Option<&str>| exported_path(value.map(OsString::from));
        AuthPathEnvironment {
            environment: export(environment),
            xdg_config_home: export(xdg_config_home),
            home: export(home),
            app_data: None,
        }
    }

    #[test]
    fn an_empty_xdg_config_home_falls_through_to_home() {
        let resolved = exports(None, Some(""), Some("/home/operator"))
            .resolve(DEFAULT_AUTH_FILE_NAME)
            .expect("an empty tier falls through to the next one");

        assert_eq!(
            resolved,
            PathBuf::from("/home/operator/.config/dekopon/chatgpt-auth.json")
        );
    }

    #[test]
    fn every_tier_being_empty_is_the_same_as_every_tier_being_unset() {
        let refused = exports(Some(""), Some(""), Some(""))
            .resolve(DEFAULT_AUTH_FILE_NAME)
            .expect_err("no tier applies");

        let ChatGptError::Configuration(message) = refused else {
            panic!("an exhausted ladder is a configuration error: {refused:?}");
        };
        assert!(
            message.contains("DEKOPON_CHATGPT_AUTH_FILE"),
            "the refusal must name the way out it accepts: {message}"
        );
    }

    #[test]
    fn a_relative_credential_path_is_refused_by_name() {
        let refused = exports(Some("dekopon-auth.json"), None, None)
            .resolve(DEFAULT_AUTH_FILE_NAME)
            .expect_err("a relative credential path is refused");

        let ChatGptError::Configuration(message) = refused else {
            panic!("a relative credential path is a configuration error: {refused:?}");
        };
        assert!(
            message.contains("dekopon-auth.json"),
            "the refusal must name the path an operator will go and look at: {message}"
        );
        assert!(
            message.contains("DEKOPON_CHATGPT_AUTH_FILE"),
            "the refusal must name the variable that produced it: {message}"
        );
    }

    #[test]
    fn a_relative_xdg_config_home_is_refused_rather_than_written_into_the_cwd() {
        let refused = exports(None, Some("relative-config"), Some("/home/operator"))
            .resolve(DEFAULT_AUTH_FILE_NAME)
            .expect_err("a relative tier is refused rather than silently used");

        let ChatGptError::Configuration(message) = refused else {
            panic!("a relative credential path is a configuration error: {refused:?}");
        };
        assert!(
            message.contains("relative-config"),
            "the refusal must name the path an operator will go and look at: {message}"
        );
    }

    #[test]
    fn an_auth_path_that_cannot_be_read_is_a_read_failure_with_its_io_kind() {
        let temp = TempDir::new().expect("temporary directory");

        let Err(ChatGptError::ReadAuth { source, .. }) = load_credentials(temp.path()) else {
            panic!("an unreadable auth path is a read failure");
        };
        assert_eq!(source.kind(), io::ErrorKind::IsADirectory);
    }

    fn fake_access(account: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": account}
            }))
            .expect("serialize JWT fixture"),
        );
        format!("header.{payload}.signature")
    }

    #[test]
    fn extracts_chatgpt_account_from_access_token() {
        assert_eq!(
            extract_account_id(&fake_access("acct-test")).expect("valid fixture"),
            "acct-test"
        );
    }

    #[test]
    fn a_malformed_access_token_reports_which_decode_failed_and_where() {
        let bad_base64 = extract_account_id("header.not base64!.signature")
            .expect_err("a non-base64 payload segment is rejected")
            .to_string();
        assert!(bad_base64.contains("invalid JWT encoding"), "{bad_base64}");
        assert!(
            bad_base64.len()
                > "invalid ChatGPT authentication response: access token has invalid JWT encoding"
                    .len(),
            "the decoder's own diagnosis is threaded through: {bad_base64}"
        );

        let payload = URL_SAFE_NO_PAD.encode(b"{\"not\": ");
        let bad_json = extract_account_id(&format!("header.{payload}.signature"))
            .expect_err("a payload that is not JSON is rejected")
            .to_string();
        assert!(bad_json.contains("invalid JWT JSON"), "{bad_json}");
        assert!(
            bad_json.contains("column"),
            "serde_json's position survives: {bad_json}"
        );
    }

    #[test]
    fn missing_credentials_point_to_the_operator_auth_command() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("missing-auth.json");
        let error = match CodexClient::new("gpt-test", Some(&path), Duration::from_secs(1), &[]) {
            Ok(_) => panic!("missing credentials must fail"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("dekopon-gatewayd auth chatgpt login")
        );
    }

    #[test]
    fn stores_credentials_without_exposing_them_in_status() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        let credentials = ChatGptCredentials {
            version: AUTH_VERSION,
            access: Redacted::new(fake_access("acct-test")),
            refresh: Redacted::new("refresh-secret".to_owned()),
            expires_at: u64::MAX,
            account_id: "acct-test".to_owned(),
        };

        save_credentials(&path, &credentials).expect("save credentials");
        let loaded = load_credentials(&path).expect("load credentials");

        assert_eq!(loaded.account_id, "acct-test");
        assert_eq!(loaded.refresh.expose(), "refresh-secret");
        let status = status(Some(&path)).expect("credential status");
        assert!(status.signed_in);
        assert!(!status.expired);
        assert!(!format!("{status:?}").contains("refresh-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn logout_removes_only_dekopons_selected_credential_file() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        let unrelated = temp.path().join("other-client.json");
        fs::write(&path, "credential").expect("write credential fixture");
        fs::write(&unrelated, "untouched").expect("write unrelated fixture");

        logout(Some(&path)).expect("logout succeeds");

        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(unrelated).expect("unrelated file remains"),
            "untouched"
        );
    }

    #[tokio::test]
    async fn builds_codex_responses_payload_with_native_items() {
        let mut assistant = crate::model::AssistantTurn::new(None, Vec::new(), None)
            .with_codex_continuation(
                vec![json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "opaque"
                })],
                crate::model::ModelKey::fixture(),
                None,
            );
        assistant.tool_calls.push(crate::model::ModelToolCall {
            id: "call-1".into(),
            kind: "function".to_owned(),
            function: crate::model::ModelFunctionCall {
                name: "echo_echo".to_owned(),
                arguments: "{}".to_owned(),
            },
        });
        let messages = vec![
            ModelMessage::system("Be concise"),
            ModelMessage::user("echo"),
            crate::model::assistant_message(&assistant),
            ModelMessage::tool("call-1", "{}"),
        ];
        let body = request_body_json(
            "gpt-test",
            &messages,
            &[ModelTool {
                name: "echo_echo".to_owned(),
                description: "Echo".to_owned(),
                parameters: json!({"type":"object"}),
            }],
            &CompletionOptions::default(),
        )
        .await
        .expect("request body");

        assert_eq!(body["instructions"], "Be concise");
        assert_eq!(body["tools"][0]["name"], "echo_echo");
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][2]["arguments"], "{}");
        assert_eq!(body["input"][3]["type"], "function_call_output");
    }

    fn request_text(fragment: &Value) -> String {
        serde_json::to_string(fragment).expect("serialize request fragment")
    }

    fn bash_tool() -> ModelTool {
        ModelTool {
            name: "bash".to_owned(),
            description: "Run a sandboxed script".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            }),
        }
    }

    fn scripted_turn(turn: u32, call_id: &str, script: &str) -> crate::model::AssistantTurn {
        tool_turn(turn, call_id, "bash", &json!({"command": script}))
    }

    fn tool_turn(
        turn: u32,
        call_id: &str,
        name: &str,
        arguments: &Value,
    ) -> crate::model::AssistantTurn {
        let arguments = arguments.to_string();
        crate::model::AssistantTurn::new(
            None,
            vec![crate::model::ModelToolCall {
                id: call_id.into(),
                kind: "function".to_owned(),
                function: crate::model::ModelFunctionCall {
                    name: name.to_owned(),
                    arguments: arguments.clone(),
                },
            }],
            None,
        )
        .with_codex_continuation(
            vec![
                json!({
                    "type": "reasoning",
                    "id": format!("rs_{turn}"),
                    "encrypted_content": "opaque",
                }),
                json!({
                    "type": "function_call",
                    "id": format!("fc_{turn}"),
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                }),
            ],
            crate::model::ModelKey::fixture(),
            None,
        )
    }

    #[tokio::test]
    async fn attachments_become_responses_input_parts() {
        let messages = [ModelMessage::user_with_parts(vec![
            ContentPart::Text("what does this say?".to_owned()),
            ContentPart::Image {
                mime: "image/png".to_owned(),
                data: crate::asset::DiskBlob::from_bytes(b"PNG")
                    .expect("spool")
                    .into(),
            },
            ContentPart::File {
                name: "spec.pdf".to_owned(),
                mime: "application/pdf".to_owned(),
                data: crate::asset::DiskBlob::from_bytes(b"PDF")
                    .expect("spool")
                    .into(),
            },
        ])];
        let cloned = messages.clone();
        for _ in 0..3 {
            let body =
                request_body_json("gpt-5-codex", &cloned, &[], &CompletionOptions::default())
                    .await
                    .expect("request body");

            assert_eq!(
                body["input"][0]["content"],
                json!([
                    {"type": "input_text", "text": "what does this say?"},
                    {"type": "input_image", "image_url": format!("data:{};base64,UE5H", "image/png")},
                    {"type": "input_file", "filename": "spec.pdf", "file_data": "data:application/pdf;base64,UERG"},
                ])
            );
        }
    }

    #[tokio::test]
    async fn every_input_item_keeps_the_shape_the_responses_api_is_sent() {
        let assistant = crate::model::AssistantTurn::new(
            Some("here you go".to_owned()),
            vec![crate::model::ModelToolCall {
                id: "call-9".into(),
                kind: "function".to_owned(),
                function: crate::model::ModelFunctionCall {
                    name: "bash".to_owned(),
                    arguments: r#"{"command":"ls"}"#.to_owned(),
                },
            }],
            None,
        );
        let messages = vec![
            ModelMessage::user("what is here?"),
            crate::model::assistant_message(&assistant),
            ModelMessage::tool("call-9", "one file"),
        ];

        let body = request_body_json(
            "gpt-test",
            &messages,
            &[bash_tool()],
            &CompletionOptions::default(),
        )
        .await
        .expect("request body");

        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "what is here?"}]},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "here you go", "annotations": []}]},
                {"type": "function_call", "call_id": "call-9", "name": "bash",
                 "arguments": "{\"command\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call-9", "output": "one file"},
            ])
        );
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "name": "bash",
                "description": "Run a sandboxed script",
                "parameters": bash_tool().parameters,
            }])
        );
        assert_eq!(body["text"], json!({"verbosity": "low"}));
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[tokio::test]
    async fn a_text_only_user_message_keeps_its_single_input_text_part() {
        let body = request_body_json(
            "gpt-5-codex",
            &[ModelMessage::user("how many files?")],
            &[],
            &CompletionOptions::default(),
        )
        .await
        .expect("request body");

        assert_eq!(
            body["input"][0]["content"],
            json!([{"type": "input_text", "text": "how many files?"}])
        );
    }

    #[tokio::test]
    async fn an_appended_turn_extends_the_request_input_and_leaves_its_prefix_untouched() {
        let tools = vec![bash_tool()];
        let mut messages = vec![
            ModelMessage::system("Be concise."),
            ModelMessage::user("how many files are in the repository?"),
        ];
        let mut bodies = vec![
            request_body_json("gpt-test", &messages, &tools, &CompletionOptions::default())
                .await
                .expect("request body"),
        ];
        messages.push(crate::model::assistant_message(&scripted_turn(
            1,
            "call_1",
            "ls | wc -l",
        )));
        messages.push(ModelMessage::tool("call_1", "12\n"));
        bodies.push(
            request_body_json("gpt-test", &messages, &tools, &CompletionOptions::default())
                .await
                .expect("request body"),
        );
        messages.push(crate::model::assistant_message(&tool_turn(
            2,
            "call_2",
            "fetch_chat_asset",
            &json!({"id": 3}),
        )));
        messages.push(ModelMessage::tool(
            "call_2",
            "chat-asset:3 follows in the next message.",
        ));
        messages.push(ModelMessage::user_with_parts(vec![
            ContentPart::Text("chat-asset:3:".to_owned()),
            ContentPart::Image {
                mime: "image/png".to_owned(),
                data: crate::asset::DiskBlob::from_bytes(b"PNG")
                    .expect("spool")
                    .into(),
            },
        ]));
        bodies.push(
            request_body_json("gpt-test", &messages, &tools, &CompletionOptions::default())
                .await
                .expect("request body"),
        );
        messages.push(crate::model::assistant_message(&scripted_turn(
            3,
            "call_3",
            "ls -a | wc -l",
        )));
        messages.push(ModelMessage::tool("call_3", "12\n"));
        bodies.push(
            request_body_json("gpt-test", &messages, &tools, &CompletionOptions::default())
                .await
                .expect("request body"),
        );

        assert_eq!(
            bodies[2]["input"]
                .as_array()
                .expect("input array")
                .iter()
                .rev()
                .take(2)
                .collect::<Vec<_>>(),
            [
                &json!({"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "chat-asset:3:"},
                    {"type": "input_image", "image_url": "data:image/png;base64,UE5H"},
                ]}),
                &json!({"type": "function_call_output", "call_id": "call_2",
                        "output": "chat-asset:3 follows in the next message."}),
            ]
        );
        for pair in bodies.windows(2) {
            let (previous, next) = (&pair[0], &pair[1]);
            assert_eq!(
                previous["instructions"], next["instructions"],
                "an appended turn rewrote the instructions that open every request"
            );
            assert_eq!(
                previous["tools"], next["tools"],
                "an appended turn rewrote the tool definitions"
            );
            let previous_input = previous["input"].as_array().expect("input array");
            let next_input = next["input"].as_array().expect("input array");
            assert!(
                next_input.len() > previous_input.len(),
                "an appended turn must extend the input rather than replace it"
            );
            assert_eq!(
                previous_input.as_slice(),
                &next_input[..previous_input.len()],
                "an earlier input item changed between turns; the cached prefix ends there"
            );
        }
    }

    #[tokio::test]
    async fn a_system_message_anywhere_in_history_rewrites_the_front_of_the_request() {
        let tools = vec![bash_tool()];
        let assistant = scripted_turn(1, "call_1", "ls | wc -l");
        let history = vec![
            ModelMessage::system("Be concise."),
            ModelMessage::user("how many files are in the repository?"),
            crate::model::assistant_message(&assistant),
            ModelMessage::tool("call_1", "12\n"),
        ];
        let mut injected = history.clone();
        injected.insert(2, ModelMessage::system("Prefer relative paths."));

        let plain = request_body_json("gpt-test", &history, &tools, &CompletionOptions::default())
            .await
            .expect("request body");
        let hoisted =
            request_body_json("gpt-test", &injected, &tools, &CompletionOptions::default())
                .await
                .expect("request body");

        assert_eq!(
            request_text(&plain["input"]),
            request_text(&hoisted["input"]),
            "the injected system message is invisible in input, which is what makes this a trap"
        );
        assert_eq!(plain["instructions"], "Be concise.");
        assert_eq!(
            hoisted["instructions"], "Be concise.\n\nPrefer relative paths.",
            "a mid-history system message is joined onto the front of the request"
        );
    }

    #[tokio::test]
    async fn a_repeated_system_message_silently_doubles_the_instructions() {
        let system = "Be concise.";
        let messages = vec![
            ModelMessage::system(system),
            ModelMessage::user("how many files are in the repository?"),
            ModelMessage::system(system),
        ];

        let body = request_body_json("gpt-test", &messages, &[], &CompletionOptions::default())
            .await
            .expect("request body");

        assert_eq!(body["instructions"], format!("{system}\n\n{system}"));
        assert_eq!(
            body["input"].as_array().expect("input array").len(),
            1,
            "neither system message reaches input, so the duplication is invisible there"
        );
    }

    #[tokio::test]
    async fn codex_requests_never_ask_the_provider_to_retain_the_conversation() {
        // store: false is a data-retention decision, not a tuning knob, since flipping it would
        // send conversation content into the provider's storage.
        let assistant = scripted_turn(1, "call_1", "ls | wc -l");
        let opening = request_body_json(
            "gpt-test",
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
        )
        .await
        .expect("request body");
        let resumed = request_body_json(
            "gpt-test",
            &[
                ModelMessage::user("how many files are in the repository?"),
                crate::model::assistant_message(&assistant),
                ModelMessage::tool("call_1", "12\n"),
            ],
            &[bash_tool()],
            &CompletionOptions::default(),
        )
        .await
        .expect("request body");

        for body in [&opening, &resumed] {
            assert_eq!(body["store"], false);
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
    }

    fn cached_conversation() -> Vec<ModelMessage> {
        let assistant = scripted_turn(1, "call_1", "ls | wc -l");
        vec![
            ModelMessage::system("Be concise."),
            ModelMessage::user("how many files are in the repository?"),
            crate::model::assistant_message(&assistant),
            ModelMessage::tool("call_1", "12\n"),
        ]
    }

    #[tokio::test]
    async fn a_request_without_a_cache_key_carries_no_cache_field_at_all() {
        let messages = cached_conversation();
        let tools = vec![bash_tool()];
        let plain = request_body_json("gpt-test", &messages, &tools, &CompletionOptions::default())
            .await
            .expect("request body");

        assert!(
            plain.get("prompt_cache_key").is_none(),
            "a keyless request grew a cache field"
        );
        assert!(
            !request_text(&plain).contains("prompt_cache_key"),
            "the field name reached the wire without a key to carry"
        );

        let blank = request_body_json(
            "gpt-test",
            &messages,
            &tools,
            &CompletionOptions::default().with_prompt_cache_key("   "),
        )
        .await
        .expect("request body");
        assert_eq!(request_text(&blank), request_text(&plain));
    }

    #[tokio::test]
    async fn a_cache_key_adds_one_field_and_disturbs_nothing_else() {
        let messages = cached_conversation();
        let tools = vec![bash_tool()];
        let plain = request_body_json("gpt-test", &messages, &tools, &CompletionOptions::default())
            .await
            .expect("request body");
        let keyed = request_body_json(
            "gpt-test",
            &messages,
            &tools,
            &CompletionOptions::default().with_prompt_cache_key("session-7"),
        )
        .await
        .expect("request body");

        assert_eq!(keyed["prompt_cache_key"], "session-7");
        let plain_fields = plain.as_object().expect("request object");
        let keyed_fields = keyed.as_object().expect("request object");
        assert_eq!(
            keyed_fields.len(),
            plain_fields.len() + 1,
            "the cache key added or removed a field other than its own"
        );
        for (field, value) in plain_fields {
            assert_eq!(
                request_text(value),
                request_text(&keyed_fields[field]),
                "the cache key rewrote {field}, which is part of the prefix it is supposed to hit"
            );
        }
    }

    #[tokio::test]
    async fn the_codex_transport_sends_a_cache_key_only_when_a_caller_supplies_one() {
        let completion = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );
        let server = MockServer::start(vec![
            MockResponse::sse(completion),
            MockResponse::sse(completion),
        ]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(
            &path,
            &ChatGptCredentials {
                version: AUTH_VERSION,
                access: Redacted::new(fake_access("acct-test")),
                refresh: Redacted::new("refresh-secret".to_owned()),
                expires_at: u64::MAX,
                account_id: "acct-test".to_owned(),
            },
        )
        .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");
        let messages = vec![ModelMessage::user("hello")];

        generate_turn(
            &model,
            &messages,
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect("keyless turn");
        generate_turn(
            &model,
            &messages,
            &[],
            &CompletionOptions::default().with_prompt_cache_key("session-7"),
            &mut ignored,
        )
        .await
        .expect("keyed turn");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0]
                .lines()
                .any(|line| line.trim_end_matches('\r').starts_with("session-id:"))
        );
        let (_, plain) = requests[0].split_once("\r\n\r\n").unwrap();
        let plain: serde_json::Value = serde_json::from_str(plain).unwrap();
        assert!(plain.get("prompt_cache_key").is_none());
        assert!(
            requests[1]
                .lines()
                .any(|line| line.trim_end_matches('\r') == "session-id: session-7")
        );
        let (_, keyed) = requests[1].split_once("\r\n\r\n").unwrap();
        let keyed: serde_json::Value = serde_json::from_str(keyed).unwrap();
        assert_eq!(keyed["prompt_cache_key"], "session-7");
    }

    #[test]
    fn parses_text_tool_calls_and_encrypted_reasoning() {
        let stream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"opaque\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo_echo\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"message\\\":\\\"hello\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo_echo\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":100},\"output_tokens\":30,\"output_tokens_details\":{\"reasoning_tokens\":7},\"total_tokens\":150}}}\n\n",
            "data: [DONE]\n\n"
        );

        let turn = replay_transcript(stream, &mut ignored).expect("valid response stream");

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.name, "echo_echo");
        assert_eq!(
            turn.tool_calls[0].function.arguments,
            r#"{"message":"hello"}"#
        );
        let items = turn.codex_items().expect("native continuation");
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items.len(), 1);
        assert_eq!(
            turn.usage,
            Some(dekopon_model_token_governor::ModelUsage {
                input_tokens: Some(120),
                cache_write_tokens: None,
                cached_input_tokens: Some(100),
                output_tokens: Some(30),
                reasoning_output_tokens: Some(7),
                total_tokens: Some(150),
            })
        );
    }

    #[test]
    fn visible_text_and_tool_calls_are_reported_while_the_turn_is_still_arriving() {
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Looking\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\" it up\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo_echo\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"message\\\":\\\"hello\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo_echo\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            "data: [DONE]\n\n"
        );
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Continue(())
        };

        let turn = replay_transcript(stream, &mut sink).expect("valid response stream");

        assert_eq!(turn.content.as_deref(), Some("Looking it up"));
        assert_eq!(
            turn.tool_calls[0].function.arguments,
            r#"{"message":"hello"}"#
        );
        assert_eq!(recorded(&events), vec!["text:Looking", "text: it up"]);
    }

    #[test]
    fn a_callback_that_breaks_abandons_the_turn_and_reports_the_interruption() {
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"half an\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\" answer\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );
        let mut events = Vec::new();
        let mut sink = |event: TurnEvent| -> ControlFlow<()> {
            events.push(event);
            ControlFlow::Break(())
        };

        let error = replay_transcript(stream, &mut sink).expect_err("the caller said stop");

        assert!(
            matches!(error, crate::error::InferenceError::Cancelled),
            "{error:?}"
        );
        assert_eq!(
            recorded(&events),
            vec!["text:half an"],
            "reading continued past the break"
        );
    }

    #[tokio::test]
    async fn a_stopped_subscription_turn_is_an_interruption_rather_than_a_request_failure() {
        let server = MockServer::start(vec![MockResponse::sse(&completion_stream("stopped"))]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-test", "refresh", u64::MAX))
            .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");

        let error = generate_turn(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut |_event: TurnEvent| -> ControlFlow<()> { ControlFlow::Break(()) },
        )
        .await
        .expect_err("the caller said stop");

        assert!(matches!(error, crate::error::InferenceError::Cancelled));
    }

    #[test]
    fn a_usage_free_completion_leaves_usage_absent() {
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );

        let turn = replay_transcript(stream, &mut ignored).expect("valid response stream");

        assert_eq!(turn.usage, None);
    }

    #[test]
    fn login_persists_after_stale_cleanup_with_stderr_logging_without_deadlock() {
        use std::{
            process::{Command, Stdio},
            sync::atomic::{AtomicUsize, Ordering},
            time::Instant,
        };
        const CHILD_ENDPOINT: &str = "DEKOPON_TEST_STDERR_LOGIN_ENDPOINT";
        const CHILD_PATH: &str = "DEKOPON_TEST_STDERR_LOGIN_PATH";
        if let Ok(endpoint) = std::env::var(CHILD_ENDPOINT) {
            struct LoggedStderr(Arc<AtomicUsize>);
            impl io::Write for LoggedStderr {
                fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    io::Write::write(&mut io::stderr(), bytes)
                }
                fn flush(&mut self) -> io::Result<()> {
                    io::Write::flush(&mut io::stderr())
                }
            }
            let writes = Arc::new(AtomicUsize::new(0));
            let logged = Arc::clone(&writes);
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_writer(move || LoggedStderr(Arc::clone(&logged)))
                .finish()
                .try_init()
                .unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let path = PathBuf::from(std::env::var_os(CHILD_PATH).unwrap());
            runtime
                .block_on(super::login_on_stderr(
                    Some(&path),
                    ChatGptEndpoints::local(&endpoint),
                ))
                .unwrap();
            assert!(
                writes.load(Ordering::SeqCst) > 0,
                "the save worker must log through stderr"
            );
            return;
        }
        let server = MockServer::start(vec![
            MockResponse::json(
                json!({"device_auth_id": "device-1", "user_code": "CODE-1234", "interval": 0}),
            ),
            MockResponse::json(
                json!({"authorization_code": "authorization-1", "code_verifier": "verifier-1"}),
            ),
            MockResponse::json(
                json!({"access_token": fake_access("acct-login"), "refresh_token": "refresh-1", "expires_in": 3600}),
            ),
        ]);
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("auth.json");
        let stale = path.with_extension("tmp-0");
        fs::write(&stale, "abandoned synthetic credential").unwrap();
        let endpoint = format!("{}/login-prefix", server.base_url());
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "chatgpt::tests::login_persists_after_stale_cleanup_with_stderr_logging_without_deadlock", "--nocapture"])
            .env(CHILD_ENDPOINT, &endpoint).env(CHILD_PATH, &path)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let completed = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = child.wait_with_output().unwrap();
        assert!(
            completed,
            "login deadlocked while its save worker logged to stderr"
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!stale.exists());
        let saved = load_credentials(&path).unwrap();
        assert_eq!(saved.account_id, "acct-login");
        assert_eq!(saved.refresh.expose(), "refresh-1");
        assert_eq!(
            server.request_uris(),
            ["device-code", "device-token", "token"].map(|part| format!("{endpoint}/{part}"))
        );
    }

    #[tokio::test]
    async fn device_login_exchanges_and_stores_credentials() {
        let access = fake_access("acct-login");
        let server = MockServer::start(vec![
            MockResponse::json(json!({
                "device_auth_id": "device-1",
                "user_code": "CODE-1234",
                "interval": 0
            })),
            MockResponse::json(json!({
                "authorization_code": "authorization-1",
                "code_verifier": "verifier-1"
            })),
            MockResponse::json(json!({
                "access_token": access,
                "refresh_token": "refresh-1",
                "expires_in": 3600
            })),
        ]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        let mut output = Vec::new();

        login_with_endpoints(
            Some(&path),
            ChatGptEndpoints::local(&server.base_url()),
            &mut output,
        )
        .await
        .expect("device login succeeds");

        let credentials = load_credentials(&path).expect("stored credentials");
        assert_eq!(credentials.account_id, "acct-login");
        assert_eq!(credentials.refresh.expose(), "refresh-1");
        let output = String::from_utf8(output).expect("UTF-8 login output");
        assert!(output.contains("CODE-1234"));
        assert_eq!(
            server.request_uris(),
            ["device-code", "device-token", "token"]
                .map(|path| format!("{}/{path}", server.base_url()))
        );
        let requests = server.requests.lock();
        assert!(requests[2].contains("grant_type=authorization_code"));
        assert!(requests[2].contains("code_verifier=verifier-1"));
    }

    #[tokio::test]
    async fn subscription_model_replays_reasoning_and_correlates_tool_results() {
        let first = include_str!("fixtures/codex-tool.sse");
        let second = include_str!("fixtures/codex-answer.sse");
        let server = MockServer::start(vec![MockResponse::sse(first), MockResponse::sse(second)]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(
            &path,
            &ChatGptCredentials {
                version: AUTH_VERSION,
                access: Redacted::new(fake_access("acct-test")),
                refresh: Redacted::new("refresh-secret".to_owned()),
                expires_at: u64::MAX,
                account_id: "acct-test".to_owned(),
            },
        )
        .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");
        let tools = vec![ModelTool {
            name: "echo_echo".to_owned(),
            description: "Echo input".to_owned(),
            parameters: json!({"type":"object"}),
        }];
        let mut messages = vec![ModelMessage::user("echo hello")];

        let tool_turn = generate_turn(
            &model,
            &messages,
            &tools,
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect("tool turn");
        assert_eq!(tool_turn.tool_calls[0].id.as_str(), "call_1");
        messages.push(crate::model::assistant_message(&tool_turn));
        messages.push(ModelMessage::tool("call_1", r#"{"message":"hello"}"#));
        let answer = generate_turn(
            &model,
            &messages,
            &tools,
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect("answer turn");

        assert_eq!(answer.content.as_deref(), Some("Echoed hello."));
        assert_eq!(answer.usage.expect("usage").input_tokens, Some(23));
        let requests = server.requests.lock();
        assert_eq!(requests.len(), 2);
        let bodies = requests
            .iter()
            .map(|request| {
                serde_json::from_str::<Value>(request.split_once("\r\n\r\n").unwrap().1).unwrap()
            })
            .collect::<Vec<_>>();
        for key in [
            "model",
            "instructions",
            "tools",
            "tool_choice",
            "store",
            "stream",
        ] {
            assert_eq!(bodies[0][key], bodies[1][key]);
        }
        assert_eq!(bodies[0]["input"][0], bodies[1]["input"][0]);
        assert_eq!(bodies[1]["input"][1]["future_field"], "preserved");
        assert_eq!(bodies[1]["input"][2]["arguments"], r#"{"message":"hello"}"#);
        assert_eq!(bodies[1]["input"][3]["call_id"], "call_1");
        assert!(requests[1].contains("opaque"));
        assert!(requests[1].contains("function_call_output"));
        assert!(requests[1].contains("call_1"));
    }

    #[tokio::test]
    async fn subscription_model_refreshes_expired_credentials_before_inference() {
        let refreshed_access = fake_access("acct-refreshed");
        let server = MockServer::start(vec![
            MockResponse::json(json!({
                "access_token": refreshed_access,
                "refresh_token": "refresh-new",
                "expires_in": 3600
            })),
            MockResponse::sse(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"refreshed\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
            )),
        ]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(
            &path,
            &ChatGptCredentials {
                version: AUTH_VERSION,
                access: Redacted::new(fake_access("acct-old")),
                refresh: Redacted::new("refresh-old".to_owned()),
                expires_at: 0,
                account_id: "acct-old".to_owned(),
            },
        )
        .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");

        let turn = generate_turn(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect("model turn");

        assert_eq!(turn.content.as_deref(), Some("refreshed"));
        let credentials = load_credentials(&path).expect("refreshed credentials persisted");
        assert_eq!(credentials.account_id, "acct-refreshed");
        assert_eq!(credentials.refresh.expose(), "refresh-new");
        let requests = server.requests.lock();
        assert!(requests[0].contains("grant_type=refresh_token"));
        assert!(requests[1].contains("chatgpt-account-id: acct-refreshed"));
    }

    fn credential_fixture(account: &str, refresh: &str, expires_at: u64) -> ChatGptCredentials {
        ChatGptCredentials {
            version: AUTH_VERSION,
            access: Redacted::new(fake_access(account)),
            refresh: Redacted::new(refresh.to_owned()),
            expires_at,
            account_id: account.to_owned(),
        }
    }

    fn completion_stream(text: &str) -> String {
        format!(
            concat!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
                "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\"}}}}\n\n"
            ),
            text
        )
    }

    #[tokio::test]
    async fn a_credential_another_process_rotated_is_adopted_rather_than_refreshed_again() {
        let server = MockServer::start(vec![MockResponse::sse(&completion_stream("adopted"))]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");
        save_credentials(
            &path,
            &credential_fixture("acct-fresh", "refresh-fresh", u64::MAX),
        )
        .expect("another process completes its refresh");

        let turn = generate_turn(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect("the adopted credential must serve the turn");

        assert_eq!(turn.content.as_deref(), Some("adopted"));
        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "the client spent a refresh token another process had already retired"
        );
        assert!(requests[0].contains("chatgpt-account-id: acct-fresh"));
        assert_eq!(
            load_credentials(&path)
                .expect("stored credentials")
                .refresh
                .expose(),
            "refresh-fresh",
            "the adopted credential was overwritten with the stale one"
        );
        assert!(
            credential_lock_path(&path)
                .expect("lock path")
                .try_exists()
                .unwrap_or(false),
            "no lock was taken around the refresh"
        );
    }

    #[tokio::test]
    async fn an_unauthorized_turn_adopts_a_newer_stored_credential_before_retrying() {
        let server = MockServer::start(vec![
            MockResponse::failure(401, json!({"error": {"code": "expired"}})),
            MockResponse::sse(&completion_stream("retried")),
        ]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(
            &path,
            &credential_fixture("acct-old", "refresh-old", u64::MAX - 1),
        )
        .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");
        save_credentials(
            &path,
            &credential_fixture("acct-fresh", "refresh-fresh", u64::MAX),
        )
        .expect("another process completes its refresh");

        let turn = generate_turn(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect("the retry must use the adopted credential");

        assert_eq!(turn.content.as_deref(), Some("retried"));
        let requests = server.requests();
        assert_eq!(
            requests.len(),
            2,
            "the forced refresh spent a retired refresh token"
        );
        assert!(requests[1].contains("chatgpt-account-id: acct-fresh"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_rotated_credential_completes_the_turn_when_the_write_fails() {
        use std::os::unix::fs::PermissionsExt as _;

        let server = MockServer::start(vec![
            MockResponse::json(json!({
                "access_token": fake_access("acct-refreshed"),
                "refresh_token": "refresh-new",
                "expires_in": 3600
            })),
            MockResponse::sse(&completion_stream("rotated")),
        ]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");

        fs::File::create(credential_lock_path(&path).expect("lock path")).expect("lock file");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o500))
            .expect("make the credential directory unwritable");
        let turn = generate_turn(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("restore the credential directory");

        let turn = turn.expect("a rotated credential must still serve the turn");
        assert_eq!(turn.content.as_deref(), Some("rotated"));
        assert!(server.requests()[1].contains("chatgpt-account-id: acct-refreshed"));
        assert_eq!(
            load_credentials(&path)
                .expect("stored credentials")
                .refresh
                .expose(),
            "refresh-old",
            "the write was supposed to have failed"
        );
    }

    #[test]
    fn saving_sweeps_abandoned_credential_temporaries() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        let abandoned = temp.path().join("auth.tmp-424242");
        let unrelated = temp.path().join("other-client.tmp-424242");
        fs::write(&abandoned, "abandoned credential").expect("write abandoned fixture");
        fs::write(&unrelated, "untouched").expect("write unrelated fixture");

        save_credentials(&path, &credential_fixture("acct-test", "refresh-secret", 0))
            .expect("save credentials");

        assert!(
            !abandoned.exists(),
            "a plaintext credential temporary survived a save"
        );
        assert_eq!(
            fs::read_to_string(unrelated).expect("unrelated file remains"),
            "untouched"
        );
        assert_eq!(
            load_credentials(&path)
                .expect("stored credentials")
                .refresh
                .expose(),
            "refresh-secret"
        );
    }

    #[test]
    fn logout_removes_abandoned_credential_temporaries_too() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        let abandoned = temp.path().join("auth.tmp-424242");
        let unrelated = temp.path().join("other-client.json");
        save_credentials(&path, &credential_fixture("acct-test", "refresh-secret", 0))
            .expect("save credentials");
        fs::write(&abandoned, "abandoned credential").expect("write abandoned fixture");
        fs::write(&unrelated, "untouched").expect("write unrelated fixture");

        logout(Some(&path)).expect("logout succeeds");

        assert!(!path.exists());
        assert!(
            !abandoned.exists(),
            "logout left a plaintext credential behind under the staging name"
        );
        assert_eq!(
            fs::read_to_string(unrelated).expect("unrelated file remains"),
            "untouched"
        );
    }

    #[tokio::test]
    async fn a_rejected_refresh_reports_the_oauth_error_code() {
        let server = MockServer::start(vec![MockResponse::failure(
            400,
            json!({"error": "invalid_grant", "error_description": "refresh token is expired"}),
        )]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
            &[],
        )
        .expect("model client");

        let error = generate_turn(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect_err("a rejected refresh must fail the turn");

        assert!(
            matches!(&error, crate::error::InferenceError::Provider(failure)
            if failure.status == Some(400)
                && failure.diagnostic.contains("invalid_grant")
                && failure.diagnostic.contains("refresh token is expired"))
        );
        let message = error.to_string();
        assert!(
            !message.contains("refresh-old"),
            "the credential reached the error message: {message}"
        );
    }

    #[tokio::test]
    async fn one_dropped_poll_does_not_abort_the_device_login() {
        let server = MockServer::start(vec![
            MockResponse::json(json!({
                "device_auth_id": "device-1",
                "user_code": "CODE-1234",
                "interval": 0
            })),
            MockResponse::hang_up(),
            MockResponse::json(json!({
                "authorization_code": "authorization-1",
                "code_verifier": "verifier-1"
            })),
            MockResponse::json(json!({
                "access_token": fake_access("acct-login"),
                "refresh_token": "refresh-1",
                "expires_in": 3600
            })),
        ]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        let mut output = Vec::new();

        login_with_endpoints(
            Some(&path),
            ChatGptEndpoints::local(&server.base_url()),
            &mut output,
        )
        .await
        .expect("a dropped poll must not end the login");

        assert_eq!(
            load_credentials(&path)
                .expect("stored credentials")
                .refresh
                .expose(),
            "refresh-1"
        );
        assert_eq!(server.requests().len(), 4);
    }

    #[tokio::test]
    async fn subscription_model_sends_required_headers_and_decodes_text() {
        let server = MockServer::start(vec![MockResponse::sse(concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        ))]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(
            &path,
            &ChatGptCredentials {
                version: AUTH_VERSION,
                access: Redacted::new(fake_access("acct-test")),
                refresh: Redacted::new("refresh-secret".to_owned()),
                expires_at: u64::MAX,
                account_id: "acct-test".to_owned(),
            },
        )
        .expect("save credentials");
        let mut endpoints = ChatGptEndpoints::local(&server.base_url());
        endpoints.responses = format!("{}/responses", server.base_url());
        let model = CodexClient::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            endpoints,
            &[],
        )
        .expect("model client");

        let turn = generate_turn(
            &model,
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
            &mut ignored,
        )
        .await
        .expect("model turn");

        assert_eq!(turn.content.as_deref(), Some("hello"));
        let request = server.requests.lock()[0].clone();
        assert!(request.contains("authorization: Bearer header."));
        assert!(request.contains("chatgpt-account-id: acct-test"));
        assert!(request.contains("originator: dekopon"));
        assert!(request.contains(concat!("user-agent: dekopon/", env!("CARGO_PKG_VERSION"))));
        assert!(request.contains("content-type: application/json; charset=utf-8"));
        let body = request
            .split_once("\r\n\r\n")
            .expect("a request body follows its headers")
            .1;
        assert!(
            request.contains(&format!("content-length: {}", body.len())),
            "{request}"
        );
        assert!(
            body.starts_with(r#"{"model":"gpt-test","store":false,"stream":true,"#),
            "{body}"
        );
        assert!(!body.contains('\n'), "the body carries pretty-printing");
    }

    fn export_fixture(path: &Path) {
        let credentials = ChatGptCredentials {
            version: AUTH_VERSION,
            access: Redacted::new(fake_access("acct-export")),
            refresh: Redacted::new("refresh-secret".to_owned()),
            expires_at: 1_700_000_000,
            account_id: "acct-export".to_owned(),
        };
        save_credentials(path, &credentials).expect("save credentials");
    }

    #[test]
    fn export_returns_the_exact_bytes_a_login_would_have_written() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        export_fixture(&path);

        let export = export_credentials(Some(&path)).expect("export credentials");

        assert_eq!(export.path(), path.as_path());
        assert_eq!(
            export.expose_document(),
            fs::read_to_string(&path).expect("read credential file")
        );
        assert!(export.expose_document().ends_with('\n'));
        assert!(export.expose_document().contains("refresh-secret"));
    }

    #[test]
    fn export_debug_rendering_stays_redacted() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        export_fixture(&path);

        let export = export_credentials(Some(&path)).expect("export credentials");

        assert!(!format!("{export:?}").contains("refresh-secret"));
        assert!(format!("{export:?}").contains("REDACTED"));
    }

    #[test]
    fn export_without_a_login_fails_with_the_login_instruction() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("missing-auth.json");

        let error = export_credentials(Some(&path)).expect_err("missing credentials must fail");

        assert!(
            error
                .to_string()
                .contains("dekopon-gatewayd auth chatgpt login")
        );
    }

    #[test]
    fn export_rejects_an_incomplete_credential_file() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        fs::write(
            &path,
            r#"{"version":1,"access":"","refresh":"","expiresAt":0,"accountId":""}"#,
        )
        .expect("write incomplete fixture");

        let error = export_credentials(Some(&path)).expect_err("incomplete credentials must fail");

        assert!(error.to_string().contains("incomplete"), "{error}");
    }

    #[test]
    fn export_rejects_a_malformed_credential_file() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        fs::write(&path, "{ not json").expect("write malformed fixture");

        let error = export_credentials(Some(&path)).expect_err("malformed credentials must fail");

        assert!(error.to_string().contains("could not parse"), "{error}");
    }

    #[tokio::test]
    async fn a_rotated_credential_is_persisted_and_adopted_without_a_second_refresh() {
        let server = MockServer::start(vec![MockResponse::json(json!({
            "access_token": fake_access("acct-rotated"),
            "refresh_token": "refresh-rotated",
            "expires_in": 3600
        }))]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        let endpoints = ChatGptEndpoints::local(&server.base_url());
        let first = Arc::new(
            CredentialFile::with_endpoints(&path, endpoints.clone())
                .expect("first holder opens the credential"),
        );
        let second = Arc::new(
            CredentialFile::with_endpoints(&path, endpoints)
                .expect("second holder opens the same credential"),
        );

        let rotated = first
            .current(&refresh_client())
            .await
            .expect("the refresh succeeds");

        assert_eq!(rotated.refresh, Some(RefreshOutcome::Rotated));
        assert_eq!(rotated.account_id, "acct-rotated");
        assert_eq!(rotated.access.expose(), &fake_access("acct-rotated"));
        let stored = load_credentials(&path).expect("stored credentials");
        assert_eq!(
            stored.refresh.expose(),
            "refresh-rotated",
            "the rotated refresh token was not written back"
        );
        assert!(
            credential_lock_path(&path)
                .expect("lock path")
                .try_exists()
                .unwrap_or(false),
            "no lock was taken around the refresh"
        );

        let adopted = second
            .current(&refresh_client())
            .await
            .expect("the second holder adopts");

        assert_eq!(adopted.refresh, Some(RefreshOutcome::Adopted));
        assert_eq!(adopted.account_id, "acct-rotated");
        assert_eq!(
            server.requests().len(),
            1,
            "the second holder spent a refresh token the first had already retired"
        );

        let reused = second
            .current(&refresh_client())
            .await
            .expect("the adopted token is still current");
        assert_eq!(reused.refresh, None);
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_refresh_that_cannot_lock_fails_without_spending_the_refresh_token() {
        let server = MockServer::start(Vec::new());
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        fs::create_dir(credential_lock_path(&path).expect("lock path")).expect("block the lock");
        let credential = Arc::new(
            CredentialFile::with_endpoints(&path, ChatGptEndpoints::local(&server.base_url()))
                .expect("the credential opens"),
        );

        let refused = credential
            .current(&refresh_client())
            .await
            .expect_err("an unlocked refresh is refused");

        assert!(
            matches!(refused, ChatGptError::LockAuth { .. }),
            "{refused:?}"
        );
        assert!(server.requests().is_empty(), "the refresh token was spent");
        assert_eq!(
            load_credentials(&path).expect("stored").refresh.expose(),
            "refresh-old"
        );
    }

    #[tokio::test]
    async fn a_rejected_refresh_grant_carries_the_oauth_code_as_a_field() {
        let refresh = "refresh-old-sentinel-7c1e";
        let access = fake_access("acct-old");
        let server = MockServer::start(vec![MockResponse::failure(
            400,
            json!({
                "error": "invalid_grant",
                "error_description": format!("token expired or revoked: {refresh} {access}"),
            }),
        )]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", refresh, 0))
            .expect("save credentials");
        let credential = Arc::new(
            CredentialFile::with_endpoints(&path, ChatGptEndpoints::local(&server.base_url()))
                .expect("the credential opens"),
        );

        let refused = credential
            .current(&refresh_client())
            .await
            .expect_err("a rejected grant must not produce a token");

        let ChatGptError::TokenRefused {
            status,
            code,
            detail,
        } = &refused
        else {
            panic!("a refused grant is a token-endpoint refusal: {refused:?}");
        };
        assert_eq!(*status, 400);
        assert_eq!(
            code.as_deref(),
            Some("invalid_grant"),
            "the refusal must carry the OAuth code a caller classifies on, as a field"
        );
        assert!(detail.contains("token expired or revoked"), "{detail}");
        assert!(!detail.contains(refresh), "{detail}");
        assert!(!detail.contains(&access), "{detail}");
        assert!(!refused.to_string().contains(refresh), "{refused}");
        assert_eq!(
            load_credentials(&path)
                .expect("stored credentials")
                .refresh
                .expose(),
            refresh,
            "a failed refresh must not rewrite the stored credential"
        );
    }

    #[tokio::test]
    async fn a_rotated_credential_is_returned_when_the_write_back_fails() {
        let (response, accepted, release) = rotation_response().held_before_headers();
        let server = MockServer::start(vec![response]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        let credential = refresh_fixture(&path, &server);
        let waiter = tokio::spawn({
            let credential = Arc::clone(&credential);
            async move { credential.current(&refresh_client()).await }
        });
        accepted.await.unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        release.send(()).unwrap();
        let rotated = waiter.await.unwrap().unwrap();
        assert_eq!(rotated.refresh, Some(RefreshOutcome::RotatedUnsaved));
        assert_eq!(rotated.access.expose(), &fake_access("acct-rotated"));
        let adopted = credential
            .force_refresh(&refresh_client(), &Redacted::new(fake_access("acct-old")))
            .await
            .unwrap();
        assert_eq!(adopted.refresh, Some(RefreshOutcome::Adopted));
        assert_eq!(adopted.access.expose(), rotated.access.expose());
        credential.drain().await;
        assert_eq!(
            server.request_uris(),
            [format!("{}/prefix/token", server.base_url())]
        );
    }

    fn refresh_client() -> RefreshClient {
        RefreshClient::new(Duration::from_secs(2), &[]).unwrap()
    }

    fn rotation_response() -> MockResponse {
        MockResponse::json(json!({
            "access_token": fake_access("acct-rotated"),
            "refresh_token": "refresh-rotated",
            "expires_in": 3600,
        }))
    }

    fn refresh_fixture(path: &Path, server: &MockServer) -> Arc<CredentialFile> {
        save_credentials(path, &credential_fixture("acct-old", "refresh-old", 0)).unwrap();
        Arc::new(
            CredentialFile::with_endpoints(
                path,
                ChatGptEndpoints::local(&format!("{}/prefix", server.base_url())),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn dropping_the_waiter_preserves_a_spent_rotation_for_another_holder() {
        let (response, accepted, release) = rotation_response().held_before_headers();
        let server = MockServer::start(vec![response]);
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("auth.json");
        let first = refresh_fixture(&path, &server);
        let second = Arc::new(
            CredentialFile::with_endpoints(
                &path,
                ChatGptEndpoints::local(&format!("{}/prefix", server.base_url())),
            )
            .unwrap(),
        );
        let waiter = tokio::spawn({
            let first = Arc::clone(&first);
            async move { first.current(&refresh_client()).await }
        });
        accepted.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        first.drain().await;
        assert_eq!(
            load_credentials(&path).unwrap().refresh.expose(),
            "refresh-rotated"
        );
        let adopted = second.current(&refresh_client()).await.unwrap();
        assert_eq!(adopted.refresh, Some(RefreshOutcome::Adopted));
        assert_eq!(adopted.access.expose(), &fake_access("acct-rotated"));
        second.drain().await;
        assert_eq!(
            server.request_uris(),
            [format!("{}/prefix/token", server.base_url())]
        );
    }

    fn generated_refresh_issuer() -> (Redacted<String>, Arc<rustls::ServerConfig>) {
        use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        let key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["localhost".to_owned()])
            .unwrap()
            .signed_by(&key, &ca)
            .unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
        (Redacted::new(ca.pem()), Arc::new(config))
    }

    #[tokio::test]
    async fn refresh_generations_replace_and_remove_roots_while_in_flight_trust_is_preserved() {
        let (root_a, server_a) = generated_refresh_issuer();
        let (root_b, server_b) = generated_refresh_issuer();
        let (response, accepted, release) = rotation_response().held_before_headers();
        let server = MockServer::start_tls(vec![
            (response, server_a),
            (
                MockResponse::json(
                    json!({"access_token": fake_access("acct-replaced"), "refresh_token": "refresh-replaced", "expires_in": 3600}),
                ),
                Arc::clone(&server_b),
            ),
            (rotation_response(), Arc::clone(&server_b)),
            (rotation_response(), server_b),
        ]);
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("auth.json");
        let credential = refresh_fixture(&path, &server);
        let original = RefreshClient::new(Duration::from_secs(2), &[root_a]).unwrap();
        let waiter = tokio::spawn({
            let credential = Arc::clone(&credential);
            let original = original.clone();
            async move { credential.current(&original).await }
        });
        accepted.await.unwrap();
        let replacement = RefreshClient::new(Duration::from_secs(2), &[root_b]).unwrap();
        let removed = refresh_client();
        release.send(()).unwrap();
        let first = waiter.await.unwrap().unwrap();
        assert_eq!(first.refresh, Some(RefreshOutcome::Rotated));
        let second = credential
            .force_refresh(&replacement, &first.access)
            .await
            .unwrap();
        assert_eq!(second.refresh, Some(RefreshOutcome::Rotated));
        assert_eq!(second.access.expose(), &fake_access("acct-replaced"));
        assert!(matches!(
            credential.force_refresh(&removed, &second.access).await,
            Err(ChatGptError::Request(_))
        ));
        assert!(matches!(
            credential.force_refresh(&original, &second.access).await,
            Err(ChatGptError::Request(_))
        ));
        credential.drain().await;
        assert_eq!(
            load_credentials(&path).unwrap().refresh.expose(),
            "refresh-replaced"
        );
        assert_eq!(
            server.request_uris(),
            vec![format!("{}/prefix/token", server.base_url()); 2]
        );
        assert_eq!(server.connections(), 4);
    }

    #[tokio::test]
    async fn concurrent_callers_spend_one_refresh_token() {
        let (response, accepted, release) = rotation_response().held_before_headers();
        let server = MockServer::start(vec![response]);
        let temp = TempDir::new().unwrap();
        let credential = refresh_fixture(&temp.path().join("auth.json"), &server);
        let first = tokio::spawn({
            let credential = Arc::clone(&credential);
            async move { credential.current(&refresh_client()).await }
        });
        accepted.await.unwrap();
        let client = refresh_client();
        let mut second = Box::pin(credential.current(&client));
        assert!(futures_util::poll!(&mut second).is_pending());
        release.send(()).unwrap();
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap();
        assert_eq!(first.refresh, Some(RefreshOutcome::Rotated));
        assert_eq!(second.refresh, Some(RefreshOutcome::Adopted));
        assert_eq!(first.access.expose(), second.access.expose());
        credential.drain().await;
        assert_eq!(
            server.request_uris(),
            [format!("{}/prefix/token", server.base_url())]
        );
    }

    #[tokio::test]
    async fn cancelling_lock_acquisition_leaves_no_refresh_task_or_request() {
        let server = MockServer::start(vec![rotation_response()]);
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("auth.json");
        let credential = refresh_fixture(&path, &server);
        let lock = PrivateFileLock::acquire(&path).unwrap();
        assert!(PrivateFileLock::try_acquire(&path).unwrap().is_none());
        let client = refresh_client();
        let mut waiter = Box::pin(credential.current(&client));
        assert!(futures_util::poll!(&mut waiter).is_pending());
        drop(waiter);
        assert!(credential.tasks.lock().is_empty());
        assert!(server.request_uris().is_empty());
        drop(lock);
        let rotated = credential.current(&refresh_client()).await.unwrap();
        assert_eq!(rotated.refresh, Some(RefreshOutcome::Rotated));
        credential.drain().await;
        assert_eq!(
            server.request_uris(),
            [format!("{}/prefix/token", server.base_url())]
        );
    }

    #[tokio::test]
    async fn a_cancelled_waiter_does_not_remove_the_response_body_deadline() {
        let (response, accepted, release) = rotation_response().stalled_after(1);
        let server = MockServer::start(vec![response]);
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("auth.json");
        let credential = refresh_fixture(&path, &server);
        let waiter = tokio::spawn({
            let credential = Arc::clone(&credential);
            async move { credential.current(&refresh_client()).await }
        });
        accepted.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        credential.drain().await;
        release.send(()).unwrap();
        assert!(credential.tasks.lock().is_empty());
        assert_eq!(
            load_credentials(&path).unwrap().refresh.expose(),
            "refresh-old"
        );
        assert_eq!(
            server.request_uris(),
            [format!("{}/prefix/token", server.base_url())]
        );
    }

    #[test]
    fn a_credential_file_reports_its_own_expiry_without_a_network_call() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-test", "refresh-secret", 0))
            .expect("save credentials");
        let credential =
            CredentialFile::with_endpoints(&path, ChatGptEndpoints::local("http://127.0.0.1:9"))
                .expect("the credential opens");

        let status = credential.status().expect("status reads the snapshot");

        assert_eq!(status.path, path);
        assert!(status.signed_in);
        assert!(status.expired);
        assert_eq!(status.expires_at, Some(0));
        assert_eq!(credential.path(), path);
    }

    #[test]
    fn a_missing_credential_file_refuses_to_open() {
        let temp = TempDir::new().expect("temporary directory");
        let refused = CredentialFile::open(&temp.path().join("absent.json"))
            .expect_err("an absent credential cannot be opened");

        assert!(matches!(refused, ChatGptError::NotLoggedIn { .. }));
    }
    #[tokio::test]
    async fn responses_released_history_is_explicit_but_io_failure_is_not_hidden() {
        struct Missing(crate::asset::BlobError);
        impl crate::asset::BlobSource for Missing {
            fn pin(&self) -> Result<crate::asset::DiskBlob, crate::asset::BlobError> {
                Err(self.0)
            }
        }
        let message_for = |error| {
            ModelMessage::user_with_parts(vec![ContentPart::Image {
                mime: "image/png".into(),
                data: crate::asset::BlobReference::new(std::sync::Arc::new(Missing(error)), 12, 7),
            }])
        };
        let message = message_for(crate::asset::BlobError::Reclaimed);
        let wire = request_body_json("test", &[message], &[], &CompletionOptions::default())
            .await
            .unwrap()
            .to_string();
        assert!(
            wire.contains("gateway: chat-asset:7 was released"),
            "{wire}"
        );
        assert!(!wire.contains("image_url"));
        let message = message_for(crate::asset::BlobError::LengthChanged);
        assert!(matches!(
            request_body_json("test", &[message], &[], &CompletionOptions::default()).await,
            Err(crate::error::InferenceError::Attachment(
                crate::asset::BlobError::LengthChanged
            ))
        ));
    }
}
