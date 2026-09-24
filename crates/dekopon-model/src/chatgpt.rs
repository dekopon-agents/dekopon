use std::{
    env,
    ffi::OsString,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use dekopon_core::Redacted;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use ureq::Agent;

use crate::model::{MAX_ERROR_BODY_BYTES, sanitize_diagnostic};
use std::io::Read as _;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
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

/// This owns the only refresh-and-persist sequence for this credential; don't reimplement it
/// elsewhere, since the refresh token rotates and a second implementation risks bricking the
/// credential.
pub struct CredentialFile {
    agent: Agent,
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
    pub fn open(auth_path: &Path, timeout: Duration) -> Result<Self, ChatGptError> {
        Self::with_endpoints(auth_path, timeout, ChatGptEndpoints::production())
    }

    pub(crate) fn with_endpoints(
        auth_path: &Path,
        timeout: Duration,
        endpoints: ChatGptEndpoints,
    ) -> Result<Self, ChatGptError> {
        if timeout.is_zero() {
            return Err(ChatGptError::Configuration(
                "model timeout must be greater than zero".to_owned(),
            ));
        }
        let credentials = load_credentials(auth_path)?;
        Ok(Self {
            agent: crate::agent(timeout),
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

    /// Still returns Ok with the in-memory token as RotatedUnsaved when persisting fails after a
    /// refresh, since the provider already invalidated the old token and erroring would strand the
    /// only working credential.
    pub fn current(&self) -> Result<ResolvedCredential, ChatGptError> {
        let mut credentials = self.snapshot();
        let refresh = self.refresh_if_needed(&mut credentials, None)?;
        Ok(ResolvedCredential {
            access: credentials.access.clone(),
            account_id: credentials.account_id.clone(),
            refresh,
        })
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

    pub(crate) fn force_refresh(
        &self,
        rejected: &Redacted<String>,
    ) -> Result<ResolvedCredential, ChatGptError> {
        let mut credentials = self.snapshot();
        let refresh = self.refresh_if_needed(&mut credentials, Some(rejected))?;
        Ok(ResolvedCredential {
            access: credentials.access,
            account_id: credentials.account_id,
            refresh,
        })
    }

    /// The credential lock must not be held across a request; holding it while streaming would
    /// serialize every caller on one client.
    fn snapshot(&self) -> ChatGptCredentials {
        self.credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// install() must only be called while holding the refresh lock, since publication must finish
    /// before another caller can rotate or adopt.
    fn install(&self, credentials: &ChatGptCredentials) {
        let mut stored = self
            .credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *stored = credentials.clone();
    }

    /// The refresh token rotates and OAuth reuse detection can revoke the whole family if a retired
    /// token is replayed, so processes serialize on a lock and the loser adopts the winner's write.
    fn refresh_if_needed(
        &self,
        credentials: &mut ChatGptCredentials,
        rejected: Option<&Redacted<String>>,
    ) -> Result<Option<RefreshOutcome>, ChatGptError> {
        if rejected.is_none() && !needs_refresh(credentials)? {
            return Ok(None);
        }
        let span = tracing::info_span!(
            "chatgpt.refresh",
            forced = rejected.is_some(),
            outcome = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            credential.expires_at = tracing::field::Empty,
        );
        let _entered = span.enter();
        let started = Instant::now();

        let _lock = CredentialLock::acquire(&self.path)?;
        // Credentials are re-read right after taking the refresh lock because another process may
        // have rotated them while this one waited.
        *credentials = self.snapshot();
        let installed_replacement = rejected
            .is_some_and(|token| credentials.access.expose() != token.expose())
            && !needs_refresh(credentials)?;
        let adopted = !installed_replacement && adopt_stored_credentials(&self.path, credentials);
        if let Some(rejected) = rejected
            && credentials.access.expose() == rejected.expose()
            && let Ok(stored) = load_credentials(&self.path)
            && stored.access.expose() != rejected.expose()
            && !needs_refresh(&stored)?
        {
            *credentials = stored;
        }
        let replaced = rejected.is_none_or(|token| credentials.access.expose() != token.expose());
        if (adopted || replaced) && !needs_refresh(credentials)? {
            self.install(credentials);
            record_refresh(
                &span,
                RefreshOutcome::Adopted.label(),
                started,
                credentials.expires_at,
            );
            return Ok(Some(RefreshOutcome::Adopted));
        }

        let refreshed = match refresh_credentials(&self.agent, &self.endpoints, credentials) {
            Ok(refreshed) => refreshed,
            Err(error) => {
                span.record("outcome", "failed");
                span.record("duration_ms", elapsed_ms(started));
                return Err(error);
            }
        };
        *credentials = refreshed;
        let outcome = match save_credentials(&self.path, credentials) {
            Ok(()) => RefreshOutcome::Rotated,
            Err(error) => {
                tracing::error!(
                    event = "chatgpt_credential_save_failed",
                    path = %self.path.display(),
                    error = %error,
                    "ChatGPT credential rotated but could not be persisted; continuing with the \
                     in-memory token"
                );
                RefreshOutcome::RotatedUnsaved
            }
        };
        self.install(credentials);
        record_refresh(&span, outcome.label(), started, credentials.expires_at);
        Ok(Some(outcome))
    }
}

pub fn login(auth_path: Option<&Path>) -> Result<PathBuf, ChatGptError> {
    login_with_output(auth_path, &mut io::stdout())
}

pub fn login_with_output(
    auth_path: Option<&Path>,
    output: &mut dyn Write,
) -> Result<PathBuf, ChatGptError> {
    login_with_endpoints(auth_path, ChatGptEndpoints::production(), output)
}

fn login_with_endpoints(
    auth_path: Option<&Path>,
    endpoints: ChatGptEndpoints,
    output: &mut dyn Write,
) -> Result<PathBuf, ChatGptError> {
    let path = resolve_auth_path(auth_path)?;
    let agent = crate::agent(Duration::from_secs(30));
    let device = start_device_login(&agent, &endpoints)?;
    writeln!(output, "Open {}", endpoints.verification_url)
        .and_then(|()| writeln!(output, "Enter code: {}", device.user_code))
        .and_then(|()| writeln!(output, "Waiting for ChatGPT authorization…"))
        .map_err(|source| ChatGptError::Output { source })?;
    output
        .flush()
        .map_err(|source| ChatGptError::Output { source })?;

    let authorization = poll_device_login(&agent, &endpoints, &device)?;
    let credentials = exchange_authorization(&agent, &endpoints, &authorization)?;
    save_credentials(&path, &credentials)?;
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

/// `logout()` must also sweep abandoned `.tmp-<pid>` siblings, since they hold the same plaintext
/// tokens the main credential file does.
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

/// Nothing in export_credentials() itself enforces it; every caller must gate the call behind an
/// explicit operator instruction and a warning.
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

fn start_device_login(
    agent: &Agent,
    endpoints: &ChatGptEndpoints,
) -> Result<DeviceLogin, ChatGptError> {
    let mut response = agent
        .post(&endpoints.device_code)
        .send_json(json!({"client_id": CLIENT_ID}))
        .map_err(|error| ChatGptError::Request(error.to_string()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let detail = oauth_failure_detail(response);
        return Err(ChatGptError::Login(format!(
            "device authorization returned HTTP {status}: {detail}"
        )));
    }
    let response = response
        .body_mut()
        .read_json::<DeviceCodeResponse>()
        .map_err(|error| ChatGptError::Protocol(error.to_string()))?;
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

fn poll_device_login(
    agent: &Agent,
    endpoints: &ChatGptEndpoints,
    device: &DeviceLogin,
) -> Result<DeviceAuthorization, ChatGptError> {
    let started = Instant::now();
    let mut interval = device.interval;
    let mut transport_failure: Option<String> = None;
    while started.elapsed() < DEVICE_LOGIN_TIMEOUT {
        let remaining = DEVICE_LOGIN_TIMEOUT.saturating_sub(started.elapsed());
        thread::sleep(interval.min(remaining));
        let response = match agent.post(&endpoints.device_token).send_json(json!({
            "device_auth_id": device.device_auth_id,
            "user_code": device.user_code,
        })) {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    event = "chatgpt_device_login_poll_failed",
                    error = %error,
                    "device authorization poll failed; continuing to poll until the deadline"
                );
                transport_failure = Some(error.to_string());
                interval = backed_off(interval);
                continue;
            }
        };
        transport_failure = None;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            let mut response = response;
            let response = response
                .body_mut()
                .read_json::<DeviceAuthorizationResponse>()
                .map_err(|error| ChatGptError::Protocol(error.to_string()))?;
            return Ok(DeviceAuthorization {
                code: Redacted::new(response.authorization_code),
                verifier: Redacted::new(response.code_verifier),
            });
        }
        let body = read_error_body(response, crate::diagnostic::DiagnosticSecrets::default());
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

fn exchange_authorization(
    agent: &Agent,
    endpoints: &ChatGptEndpoints,
    authorization: &DeviceAuthorization,
) -> Result<ChatGptCredentials, ChatGptError> {
    request_token(
        agent,
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
}

fn refresh_credentials(
    agent: &Agent,
    endpoints: &ChatGptEndpoints,
    credentials: &ChatGptCredentials,
) -> Result<ChatGptCredentials, ChatGptError> {
    request_token(
        agent,
        &endpoints.token,
        [
            ("grant_type", "refresh_token"),
            ("refresh_token", credentials.refresh.expose().as_str()),
            ("client_id", CLIENT_ID),
        ],
        crate::diagnostic::DiagnosticSecrets::new(Some(&credentials.refresh))
            .with_previous(&credentials.access),
    )
}

fn request_token<'a, const N: usize>(
    agent: &Agent,
    endpoint: &str,
    form: [(&'a str, &'a str); N],
    secrets: crate::diagnostic::DiagnosticSecrets<'_>,
) -> Result<ChatGptCredentials, ChatGptError> {
    let mut response = agent
        .post(endpoint)
        .send_form(form)
        .map_err(|error| ChatGptError::Request(secrets.sanitize(&error.to_string())))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = read_error_body(response, secrets);
        let code = oauth_error_code(&body).map(|value| secrets.sanitize(&value));
        return Err(ChatGptError::TokenRefused {
            status,
            detail: secrets.sanitize(&oauth_detail(&body, code.as_deref())),
            code,
        });
    }
    let token = response
        .body_mut()
        .read_json::<TokenResponse>()
        .map_err(|error| ChatGptError::Protocol(secrets.sanitize(&error.to_string())))?;
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

fn read_error_body(
    response: ureq::http::Response<ureq::Body>,
    secrets: crate::diagnostic::DiagnosticSecrets<'_>,
) -> String {
    let mut bytes = Vec::new();
    let read = response
        .into_parts()
        .1
        .into_reader()
        .take(MAX_ERROR_BODY_BYTES + 1)
        .read_to_end(&mut bytes);
    let text = if read.is_err() || bytes.len() > MAX_ERROR_BODY_BYTES as usize {
        secrets.sanitize_truncated(&bytes)
    } else {
        secrets.sanitize(&String::from_utf8_lossy(&bytes))
    };
    if text.trim().is_empty() {
        return "no response body".to_owned();
    }
    text
}

fn oauth_failure_detail(response: ureq::http::Response<ureq::Body>) -> String {
    let body = read_error_body(response, crate::diagnostic::DiagnosticSecrets::default());
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

    /// Requires an absolute path, since save_credentials would happily write to a relative one,
    /// silently landing the refresh token in whatever directory the process started in.
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

/// An empty exported value counts as unset, since otherwise an empty XDG_CONFIG_HOME would resolve
/// to a relative path instead of falling through to HOME.
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

/// adopt_stored_credentials() must only be called while the refresh lock is held, or adoption could
/// race a concurrent rotation.
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

/// The lock lives on a separate sibling file, not the credential itself, since the credential is
/// replaced by rename on each refresh and would otherwise leave processes locking different inodes.
struct CredentialLock {
    file: File,
}

impl CredentialLock {
    /// A failed lock fails the whole refresh rather than proceeding uncoordinated, since an
    /// uncoordinated refresh would spend an already-retired refresh token.
    fn acquire(auth_path: &Path) -> Result<Self, ChatGptError> {
        let path = credential_lock_path(auth_path).ok_or_else(|| ChatGptError::LockAuth {
            path: auth_path.to_path_buf(),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        })?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_file_mode(&mut options);
        let file = options
            .open(&path)
            .and_then(|file| file.lock().map(|()| file))
            .map_err(|source| ChatGptError::LockAuth {
                path: path.clone(),
                source,
            })?;
        Ok(Self { file })
    }
}

impl Drop for CredentialLock {
    fn drop(&mut self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "a destructor has no caller to report to, and closing the file releases the \
                      lock regardless of what an explicit unlock answers"
        )]
        let _ = self.file.unlock();
    }
}

fn credential_lock_path(auth_path: &Path) -> Option<PathBuf> {
    let name = auth_path.file_name()?;
    let mut lock_name = OsString::from(name);
    lock_name.push(".lock");
    Some(auth_path.with_file_name(lock_name))
}

fn load_credentials(path: &Path) -> Result<ChatGptCredentials, ChatGptError> {
    let file = File::open(path).map_err(|source| {
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
    let credentials = serde_json::from_reader::<_, ChatGptCredentials>(file).map_err(|source| {
        ChatGptError::ParseAuth {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if credentials.version != AUTH_VERSION {
        return Err(ChatGptError::Configuration(format!(
            "unsupported ChatGPT credential version {}",
            credentials.version
        )));
    }
    if credentials.access.expose().is_empty()
        || credentials.refresh.expose().is_empty()
        || credentials.account_id.is_empty()
    {
        return Err(ChatGptError::Configuration(
            "ChatGPT credential file is incomplete".to_owned(),
        ));
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

    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    // Sweeps stale temp files first, since a SIGKILL between create and rename leaves plaintext
    // credentials that would otherwise accumulate forever on a persistent volume.
    sweep_stale_temporaries(path, Some(&temporary));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_mode(&mut options);
    let result = (|| {
        let mut file = options
            .open(&temporary)
            .map_err(|source| ChatGptError::WriteAuth {
                path: temporary.clone(),
                source,
            })?;
        serde_json::to_writer(&mut file, credentials).map_err(|source| {
            ChatGptError::SerializeAuth {
                path: temporary.clone(),
                source,
            }
        })?;
        file.write_all(b"\n")
            .and_then(|()| file.sync_all())
            .map_err(|source| ChatGptError::WriteAuth {
                path: temporary.clone(),
                source,
            })?;
        replace_file(&temporary, path).map_err(|source| ChatGptError::WriteAuth {
            path: path.to_path_buf(),
            source,
        })?;
        // Without this directory sync, the rename can be lost on power failure, leaving only the
        // already-invalidated predecessor credential on disk.
        sync_directory(parent).map_err(|source| ChatGptError::WriteAuth {
            path: parent.to_path_buf(),
            source,
        })
    })();
    if result.is_err() {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "rollback of a temporary the write already failed on; the caller is being \
                      given that write error, and a leftover 0600 temporary is not worth \
                      replacing it with a cleanup error"
        )]
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Deleting matched .tmp-* siblings is safe only because refreshes serialize on the lock and logins
/// are human-paced, never concurrent.
fn sweep_stale_temporaries(path: &Path, keep: Option<&Path>) {
    let (Some(parent), Some(stem)) = (path.parent(), path.file_stem()) else {
        return;
    };
    let mut prefix = OsString::from(stem);
    prefix.push(".tmp-");
    let Some(prefix) = prefix.to_str().map(ToOwned::to_owned) else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry.path();
        if Some(stale.as_path()) == keep {
            continue;
        }
        let Some(name) = stale.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        if let Err(error) = fs::remove_file(&stale) {
            tracing::warn!(
                event = "chatgpt_credential_temporary_orphaned",
                path = %stale.display(),
                error = %error,
                "could not remove an abandoned ChatGPT credential temporary file"
            );
        } else {
            tracing::warn!(
                event = "chatgpt_credential_temporary_swept",
                path = %stale.display(),
                "removed an abandoned ChatGPT credential temporary file"
            );
        }
    }
}

#[cfg(unix)]
fn sync_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_parent: &Path) -> io::Result<()> {
    // Windows can't open a directory as a file and its rename isn't the same durability contract,
    // so this platform only gets the file's own sync_all.
    Ok(())
}

#[cfg(unix)]
fn set_private_file_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn set_private_file_mode(_options: &mut OpenOptions) {}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    match fs::remove_file(destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(temporary, destination)
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

#[derive(Debug, Error)]
pub enum ChatGptError {
    #[error("invalid ChatGPT configuration: {0}")]
    Configuration(String),
    #[error("not logged in to ChatGPT; run `dekopond auth chatgpt login` (expected {})", path.display())]
    NotLoggedIn { path: PathBuf },
    #[error("could not read ChatGPT credentials at {}", path.display())]
    ReadAuth {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse ChatGPT credentials at {}", path.display())]
    ParseAuth {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
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
    #[error("ChatGPT authentication request failed: {0}")]
    Request(String),
    #[error("invalid ChatGPT authentication response: {0}")]
    Protocol(String),
    #[error("ChatGPT login failed: {0}")]
    Login(String),
    /// invalid_grant, refresh_token_reused, refresh_token_invalidated, and refresh_token_expired
    /// mean the token family is dead and need a new device login; any other code, or a codeless
    /// 5xx, is the endpoint's problem.
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
    use std::{fs, path::Path, time::Duration};

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::codex::{CodexClient, replay_transcript, request_body_json};
    use crate::{
        control::TurnControl,
        inference::{GenerateRequest, InferenceModel},
    };
    use dekopon_core::Redacted;
    use std::ops::ControlFlow;

    use super::{
        AUTH_VERSION, AuthPathEnvironment, ChatGptCredentials, ChatGptEndpoints, ChatGptError,
        CredentialFile, DEFAULT_AUTH_FILE_NAME, OsString, PathBuf, RefreshOutcome,
        credential_lock_path, export_credentials, exported_path, extract_account_id,
        load_credentials, login_with_endpoints, logout, save_credentials, status,
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
                TurnEvent::ToolCallStarted { index } => format!("call:{index}"),
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
        let error = match CodexClient::new("gpt-test", Some(&path), Duration::from_secs(1)) {
            Ok(_) => panic!("missing credentials must fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("dekopond auth chatgpt login"));
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
                crate::model::ClientIdentity::new(),
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
        assert_eq!(body["input"][2]["type"], "function_call_output");
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
                "properties": {"script": {"type": "string"}},
                "required": ["script"],
            }),
        }
    }

    fn scripted_turn(turn: u32, call_id: &str, script: &str) -> crate::model::AssistantTurn {
        let arguments = json!({"script": script}).to_string();
        crate::model::AssistantTurn::new(
            None,
            vec![crate::model::ModelToolCall {
                id: call_id.into(),
                kind: "function".to_owned(),
                function: crate::model::ModelFunctionCall {
                    name: "bash".to_owned(),
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
                    "name": "bash",
                    "arguments": arguments,
                }),
            ],
            crate::model::ClientIdentity::new(),
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
                    arguments: r#"{"script":"ls"}"#.to_owned(),
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
                 "arguments": "{\"script\":\"ls\"}"},
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
        for (turn, script) in [(1, "ls | wc -l"), (2, "ls -a | wc -l")] {
            let call_id = format!("call_{turn}");
            let assistant = scripted_turn(turn, &call_id, script);
            messages.push(crate::model::assistant_message(&assistant));
            messages.push(ModelMessage::tool(call_id.as_str(), "12\n"));
            bodies.push(
                request_body_json("gpt-test", &messages, &tools, &CompletionOptions::default())
                    .await
                    .expect("request body"),
            );
        }

        for pair in bodies.windows(2) {
            let (previous, next) = (&pair[0], &pair[1]);
            assert_eq!(
                request_text(&previous["instructions"]),
                request_text(&next["instructions"]),
                "an appended turn rewrote the instructions that open every request"
            );
            assert_eq!(
                request_text(&previous["tools"]),
                request_text(&next["tools"]),
                "an appended turn rewrote the tool definitions"
            );
            let previous_input = previous["input"].as_array().expect("input array");
            let next_input = next["input"].as_array().expect("input array");
            assert!(
                next_input.len() > previous_input.len(),
                "an appended turn must extend the input rather than replace it"
            );
            for (index, item) in previous_input.iter().enumerate() {
                assert_eq!(
                    request_text(item),
                    request_text(&next_input[index]),
                    "input item {index} changed between turns; the cached prefix ends there"
                );
            }
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

        let requests = server.requests.lock().expect("request lock");
        assert!(
            !requests[0].contains("prompt_cache_key"),
            "complete sent a cache key nobody asked for"
        );
        assert!(requests[1].contains("prompt_cache_key"));
        assert!(requests[1].contains("session-7"));
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
        assert_eq!(items[1]["arguments"], r#"{"message":"hello"}"#);
        assert_eq!(
            turn.usage,
            Some(crate::model::ModelUsage {
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
        assert_eq!(
            recorded(&events),
            vec!["text:Looking", "text: it up", "call:0"]
        );
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
    fn device_login_exchanges_and_stores_credentials() {
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
        .expect("device login succeeds");

        let credentials = load_credentials(&path).expect("stored credentials");
        assert_eq!(credentials.account_id, "acct-login");
        assert_eq!(credentials.refresh.expose(), "refresh-1");
        let output = String::from_utf8(output).expect("UTF-8 login output");
        assert!(output.contains("CODE-1234"));
        let requests = server.requests.lock().expect("request lock");
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
        let requests = server.requests.lock().expect("request lock");
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
        let requests = server.requests.lock().expect("request lock");
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

    #[test]
    fn one_dropped_poll_does_not_abort_the_device_login() {
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
        let model =
            CodexClient::with_endpoints("gpt-test", Some(&path), Duration::from_secs(2), endpoints)
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
        let request = server.requests.lock().expect("request lock")[0].clone();
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

        assert!(error.to_string().contains("dekopond auth chatgpt login"));
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

    #[test]
    fn a_rotated_credential_is_persisted_and_adopted_without_a_second_refresh() {
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
        let first =
            CredentialFile::with_endpoints(&path, Duration::from_secs(2), endpoints.clone())
                .expect("first holder opens the credential");
        let second = CredentialFile::with_endpoints(&path, Duration::from_secs(2), endpoints)
            .expect("second holder opens the same credential");

        let rotated = first.current().expect("the refresh succeeds");

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

        let adopted = second.current().expect("the second holder adopts");

        assert_eq!(adopted.refresh, Some(RefreshOutcome::Adopted));
        assert_eq!(adopted.account_id, "acct-rotated");
        assert_eq!(
            server.requests().len(),
            1,
            "the second holder spent a refresh token the first had already retired"
        );

        let reused = second
            .current()
            .expect("the adopted token is still current");
        assert_eq!(reused.refresh, None);
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn a_refresh_that_cannot_lock_fails_without_spending_the_refresh_token() {
        let server = MockServer::start(Vec::new());
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        fs::create_dir(credential_lock_path(&path).expect("lock path")).expect("block the lock");
        let credential = CredentialFile::with_endpoints(
            &path,
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
        )
        .expect("the credential opens");

        let refused = credential
            .current()
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

    #[test]
    fn a_rejected_refresh_grant_carries_the_oauth_code_as_a_field() {
        let server = MockServer::start(vec![MockResponse::failure(
            400,
            json!({"error": "invalid_grant", "error_description": "token expired or revoked"}),
        )]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        let credential = CredentialFile::with_endpoints(
            &path,
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
        )
        .expect("the credential opens");

        let refused = credential
            .current()
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
        assert_eq!(
            load_credentials(&path)
                .expect("stored credentials")
                .refresh
                .expose(),
            "refresh-old",
            "a failed refresh must not rewrite the stored credential"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_rotated_credential_is_returned_when_the_write_back_fails() {
        use std::os::unix::fs::PermissionsExt as _;

        let server = MockServer::start(vec![MockResponse::json(json!({
            "access_token": fake_access("acct-unsaved"),
            "refresh_token": "refresh-unsaved",
            "expires_in": 3600
        }))]);
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-old", "refresh-old", 0))
            .expect("save credentials");
        let credential = CredentialFile::with_endpoints(
            &path,
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
        )
        .expect("the credential opens");

        fs::File::create(credential_lock_path(&path).expect("lock path")).expect("lock file");
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o500))
            .expect("make the credential directory unwritable");
        let resolved = credential.current();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
            .expect("restore the credential directory");

        let resolved = resolved.expect("an unsaved rotation still yields a usable token");
        assert_eq!(resolved.refresh, Some(RefreshOutcome::RotatedUnsaved));
        assert_eq!(resolved.account_id, "acct-unsaved");
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
    fn a_credential_file_reports_its_own_expiry_without_a_network_call() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        save_credentials(&path, &credential_fixture("acct-test", "refresh-secret", 0))
            .expect("save credentials");
        let credential = CredentialFile::with_endpoints(
            &path,
            Duration::from_secs(2),
            ChatGptEndpoints::local("http://127.0.0.1:9"),
        )
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
        let refused =
            CredentialFile::open(&temp.path().join("absent.json"), Duration::from_secs(2))
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
            wire.contains("gateway: Chat Asset #7 was released"),
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
