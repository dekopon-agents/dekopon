//! ChatGPT/Codex subscription authentication and Responses transport.
//!
//! The implementation uses OpenAI's public Codex device authorization flow. Credentials are
//! isolated in Dekopon's own auth file; credentials owned by other clients are never imported.

use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
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
use ureq::{Agent, http};

use crate::model::{
    AssistantTurn, ChatModel, CompletionOptions, ModelError, ModelFunctionCall, ModelMessage,
    ModelTool, ModelToolCall, ModelUsage,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE_URL: &str = "https://auth.openai.com";
const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REFRESH_MARGIN: Duration = Duration::from_secs(60);
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
const AUTH_VERSION: u32 = 1;
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// Result of inspecting Dekopon's ChatGPT subscription credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatGptAuthStatus {
    /// Credential file owned by Dekopon.
    pub path: PathBuf,
    /// Whether credentials are present.
    pub signed_in: bool,
    /// Whether the current access token has expired.
    pub expired: bool,
}

/// ChatGPT subscription model backed by OpenAI's Codex Responses endpoint.
pub struct ChatGptCodexModel {
    agent: Agent,
    model: String,
    auth_path: PathBuf,
    credentials: Mutex<ChatGptCredentials>,
    endpoints: ChatGptEndpoints,
}

impl ChatGptCodexModel {
    /// Loads Dekopon's own ChatGPT credentials and creates a bounded client.
    pub fn new(
        model: impl Into<String>,
        auth_path: Option<&Path>,
        timeout: Duration,
    ) -> Result<Self, ChatGptError> {
        Self::with_endpoints(model, auth_path, timeout, ChatGptEndpoints::production())
    }

    fn with_endpoints(
        model: impl Into<String>,
        auth_path: Option<&Path>,
        timeout: Duration,
        endpoints: ChatGptEndpoints,
    ) -> Result<Self, ChatGptError> {
        if timeout.is_zero() {
            return Err(ChatGptError::Configuration(
                "model timeout must be greater than zero".to_owned(),
            ));
        }
        let model = model.into();
        if model.trim().is_empty() {
            return Err(ChatGptError::Configuration(
                "model name must not be empty".to_owned(),
            ));
        }
        let auth_path = resolve_auth_path(auth_path)?;
        let credentials = load_credentials(&auth_path)?;
        let config = Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .http_status_as_error(false)
            .build();

        Ok(Self {
            agent: config.into(),
            model,
            auth_path,
            credentials: Mutex::new(credentials),
            endpoints,
        })
    }

    fn refresh_if_needed(
        &self,
        credentials: &mut ChatGptCredentials,
        force: bool,
    ) -> Result<(), ChatGptError> {
        let refresh_at = credentials
            .expires_at
            .saturating_sub(REFRESH_MARGIN.as_secs());
        if !force && unix_time()? < refresh_at {
            return Ok(());
        }
        *credentials =
            refresh_credentials(&self.agent, &self.endpoints, credentials.refresh.expose())?;
        save_credentials(&self.auth_path, credentials)
    }

    fn request_turn(
        &self,
        credentials: &ChatGptCredentials,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
    ) -> Result<AssistantTurn, ChatGptRequestError> {
        let body = build_request_body(&self.model, messages, tools, options);
        let response = self
            .agent
            .post(&self.endpoints.responses)
            .header(
                "authorization",
                &format!("Bearer {}", credentials.access.expose()),
            )
            .header("chatgpt-account-id", &credentials.account_id)
            .header("originator", "dekopon")
            .header(
                "user-agent",
                &format!("dekopon-run/{}", env!("CARGO_PKG_VERSION")),
            )
            .header("openai-beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .send_json(&body)
            .map_err(|error| ChatGptRequestError::Transport(error.to_string()))?;

        let status = response.status().as_u16();
        if status == 401 {
            return Err(ChatGptRequestError::Unauthorized);
        }
        if !(200..300).contains(&status) {
            let detail = read_error_body(response);
            return Err(ChatGptRequestError::Status { status, detail });
        }

        parse_sse(response.into_parts().1.into_reader())
            .map_err(|error| ChatGptRequestError::Protocol(error.to_string()))
    }
}

impl ChatModel for ChatGptCodexModel {
    fn complete(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
    ) -> Result<AssistantTurn, ModelError> {
        self.complete_with(messages, tools, &CompletionOptions::default())
    }

    fn complete_with(
        &self,
        messages: &[ModelMessage],
        tools: &[ModelTool],
        options: &CompletionOptions,
    ) -> Result<AssistantTurn, ModelError> {
        let span = tracing::info_span!(
            "model.complete",
            model = %self.model,
            model.backend = "chatgpt-subscription",
            message.count = messages.len(),
            tool.count = tools.len()
        );
        let _entered = span.enter();
        let mut credentials = self
            .credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.refresh_if_needed(&mut credentials, false)
            .map_err(|error| ModelError::Request(error.to_string()))?;

        match self.request_turn(&credentials, messages, tools, options) {
            Ok(turn) => Ok(turn),
            Err(ChatGptRequestError::Unauthorized) => {
                self.refresh_if_needed(&mut credentials, true)
                    .map_err(|error| ModelError::Request(error.to_string()))?;
                self.request_turn(&credentials, messages, tools, options)
                    .map_err(|error| ModelError::Request(error.to_string()))
            }
            Err(error) => Err(ModelError::Request(error.to_string())),
        }
    }
}

/// Performs a device-code login, writes instructions to standard output, and stores credentials.
pub fn login(auth_path: Option<&Path>) -> Result<PathBuf, ChatGptError> {
    login_with_output(auth_path, &mut io::stdout())
}

/// Performs a device-code login while writing authorization instructions to `output`.
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
    let config = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build();
    let agent: Agent = config.into();
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

/// Inspects Dekopon's ChatGPT credential store without revealing credentials.
pub fn status(auth_path: Option<&Path>) -> Result<ChatGptAuthStatus, ChatGptError> {
    let path = resolve_auth_path(auth_path)?;
    let credentials = match load_credentials(&path) {
        Ok(credentials) => credentials,
        Err(ChatGptError::NotLoggedIn { .. }) => {
            return Ok(ChatGptAuthStatus {
                path,
                signed_in: false,
                expired: false,
            });
        }
        Err(error) => return Err(error),
    };
    Ok(ChatGptAuthStatus {
        path,
        signed_in: true,
        expired: credentials.expires_at <= unix_time()?,
    })
}

/// Deletes only Dekopon's ChatGPT credential file.
pub fn logout(auth_path: Option<&Path>) -> Result<PathBuf, ChatGptError> {
    let path = resolve_auth_path(auth_path)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path),
        Err(source) => Err(ChatGptError::RemoveAuth {
            path: path.clone(),
            source,
        }),
    }
}

/// Dekopon's ChatGPT credentials, read back in the clear for a deliberate operator export.
///
/// Every other path in Dekopon keeps this material inside [`Redacted`], and the `0600` credential
/// file is the only destination trusted to hold it in the clear. This type is the second
/// exception, and it exists for one reason: device authorization needs a human at a browser, so a
/// containerized `dekopond` can only ever receive a credential an operator carried out of a local
/// login.
///
/// The document stays wrapped, so `Debug` still renders a marker. It leaves only through the
/// deliberately conspicuous [`ChatGptCredentialExport::expose_document`].
#[derive(Debug)]
pub struct ChatGptCredentialExport {
    path: PathBuf,
    document: Redacted<String>,
}

impl ChatGptCredentialExport {
    /// Path the credentials were read from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the credential document in the clear.
    ///
    /// Named to be conspicuous at call sites and in review, exactly like [`Redacted::expose`]:
    /// every use is a place where a live ChatGPT access token and a rotating refresh token leave
    /// their wrapper.
    #[must_use]
    pub fn expose_document(&self) -> &str {
        self.document.expose()
    }
}

/// Reads Dekopon's ChatGPT credentials back as the exact document a login writes.
///
/// This is a credential read rather than a status check: the returned document carries a live
/// access token and a *rotating* refresh token. Each refresh mints a replacement and invalidates
/// its predecessor, so an exported copy is stale the moment the credential it came from refreshes.
/// A caller must gate this behind an explicit operator instruction and must say that out loud;
/// [`crate::chatgpt`]'s operator surface, `dekopon auth chatgpt export`, requires
/// `--expose-credential`, refuses a terminal destination, and warns on standard error.
///
/// The bytes are identical to what [`login`] would have written, so a file seeded from this
/// document is indistinguishable from a locally created one.
///
/// # Errors
///
/// Returns [`ChatGptError::NotLoggedIn`] when no credential file exists, [`ChatGptError::ReadAuth`]
/// when one exists but cannot be read, [`ChatGptError::ParseAuth`] when it is not credential JSON,
/// and [`ChatGptError::Configuration`] when it is an unsupported version or is missing a required
/// field. Every one of those fails instead of emitting a partial document.
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
struct ChatGptEndpoints {
    device_code: String,
    device_token: String,
    token: String,
    verification_url: String,
    responses: String,
}

impl ChatGptEndpoints {
    fn production() -> Self {
        Self {
            device_code: format!("{AUTH_BASE_URL}/api/accounts/deviceauth/usercode"),
            device_token: format!("{AUTH_BASE_URL}/api/accounts/deviceauth/token"),
            token: format!("{AUTH_BASE_URL}/oauth/token"),
            verification_url: format!("{AUTH_BASE_URL}/codex/device"),
            responses: RESPONSES_URL.to_owned(),
        }
    }

    #[cfg(test)]
    fn local(base: &str) -> Self {
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

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatGptCredentials {
    version: u32,
    // The auth file is the one destination that must round-trip these in the clear. Opting in per
    // field keeps the default safe: any other struct these end up in redacts them automatically.
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
        return Err(ChatGptError::Login(format!(
            "device authorization returned HTTP {status}"
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
    while started.elapsed() < DEVICE_LOGIN_TIMEOUT {
        let remaining = DEVICE_LOGIN_TIMEOUT.saturating_sub(started.elapsed());
        thread::sleep(interval.min(remaining));
        let response = agent
            .post(&endpoints.device_token)
            .send_json(json!({
                "device_auth_id": device.device_auth_id,
                "user_code": device.user_code,
            }))
            .map_err(|error| ChatGptError::Request(error.to_string()))?;
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
        let body = read_error_body(response);
        let error_code = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| value.get("error").cloned())
            .and_then(|error| match error {
                Value::String(code) => Some(code),
                Value::Object(object) => object
                    .get("code")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                _ => None,
            });
        if status == 403
            || status == 404
            || error_code.as_deref() == Some("deviceauth_authorization_pending")
        {
            continue;
        }
        if error_code.as_deref() == Some("slow_down") || status == 429 {
            interval = interval
                .saturating_add(Duration::from_secs(5))
                .min(Duration::from_secs(30));
            continue;
        }
        return Err(ChatGptError::Login(format!(
            "device authorization failed with HTTP {status}"
        )));
    }
    Err(ChatGptError::LoginTimeout)
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
    )
}

fn refresh_credentials(
    agent: &Agent,
    endpoints: &ChatGptEndpoints,
    refresh: &str,
) -> Result<ChatGptCredentials, ChatGptError> {
    request_token(
        agent,
        &endpoints.token,
        [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT_ID),
        ],
    )
}

fn request_token<'a, const N: usize>(
    agent: &Agent,
    endpoint: &str,
    form: [(&'a str, &'a str); N],
) -> Result<ChatGptCredentials, ChatGptError> {
    let mut response = agent
        .post(endpoint)
        .send_form(form)
        .map_err(|error| ChatGptError::Request(error.to_string()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(ChatGptError::Login(format!(
            "token endpoint returned HTTP {status}"
        )));
    }
    let token = response
        .body_mut()
        .read_json::<TokenResponse>()
        .map_err(|error| ChatGptError::Protocol(error.to_string()))?;
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
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| ChatGptError::Protocol("access token has invalid JWT encoding".to_owned()))?;
    let payload = serde_json::from_slice::<Value>(&bytes)
        .map_err(|_| ChatGptError::Protocol("access token has invalid JWT JSON".to_owned()))?;
    payload
        .get(JWT_AUTH_CLAIM)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ChatGptError::Protocol("access token omitted ChatGPT account ID".to_owned()))
}

fn build_request_body(
    model: &str,
    messages: &[ModelMessage],
    tools: &[ModelTool],
    options: &CompletionOptions,
) -> Value {
    let instructions = messages
        .iter()
        .filter(|message| message.role() == "system")
        .filter_map(ModelMessage::content)
        .collect::<Vec<_>>()
        .join("\n\n");
    let instructions = if instructions.trim().is_empty() {
        "You are a helpful assistant. Use only the supplied function tools when a tool is needed."
            .to_owned()
    } else {
        instructions
    };
    let mut input = Vec::new();
    for message in messages {
        match message.role() {
            "system" => {}
            "user" => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": message.content().unwrap_or_default()}],
            })),
            "assistant" if !message.replay_items().is_empty() => {
                input.extend(message.replay_items().iter().cloned());
            }
            "assistant" => {
                if let Some(content) = message.content().filter(|content| !content.is_empty()) {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": content, "annotations": []}],
                    }));
                }
                for call in message.tool_calls() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    }));
                }
            }
            "tool" => input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id().unwrap_or_default(),
                "output": message.content().unwrap_or_default(),
            })),
            _ => {}
        }
    }
    let tools = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            })
        })
        .collect::<Vec<_>>();

    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "include": ["reasoning.encrypted_content"],
        "text": {"verbosity": "low"},
    });
    // Added only when a key exists, so a keyless request is byte-for-byte the request this
    // transport sent before the field existed. `prompt_cache_key` routes toward a warm prefix and
    // authorizes nothing; the conversation itself is already in `input` either way.
    if let Some(key) = options.prompt_cache_key()
        && let Some(object) = body.as_object_mut()
    {
        object.insert("prompt_cache_key".to_owned(), Value::String(key.to_owned()));
    }
    body
}

#[derive(Default)]
struct StreamState {
    text: String,
    replay_items: Vec<Value>,
    calls: BTreeMap<String, PendingCall>,
    call_order: Vec<String>,
    completed: bool,
    usage: Option<ModelUsage>,
}

/// Responses-API `usage` object from the `response.completed` event. Every field defaults for the
/// same reason as the chat-completions shape: a partial report still prices the call.
#[derive(Debug, Deserialize)]
struct WireResponsesUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    input_tokens_details: Option<WireInputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<WireOutputTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct WireInputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireOutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<WireResponsesUsage> for ModelUsage {
    fn from(usage: WireResponsesUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage
                .input_tokens_details
                .and_then(|details| details.cached_tokens),
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage
                .output_tokens_details
                .and_then(|details| details.reasoning_tokens),
            total_tokens: usage.total_tokens,
        }
    }
}

#[derive(Default)]
struct PendingCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
    replayed: bool,
}

fn parse_sse(reader: impl Read) -> Result<AssistantTurn, ChatGptError> {
    let mut reader = BufReader::new(reader.take(MAX_RESPONSE_BYTES.saturating_add(1)));
    let mut state = StreamState::default();
    let mut event_data = String::new();
    let mut bytes_read = 0_u64;
    loop {
        let mut line = String::new();
        let length = reader
            .read_line(&mut line)
            .map_err(|source| ChatGptError::Stream { source })?;
        if length == 0 {
            process_sse_data(&event_data, &mut state)?;
            break;
        }
        bytes_read = bytes_read.saturating_add(length as u64);
        if bytes_read > MAX_RESPONSE_BYTES {
            return Err(ChatGptError::Protocol(format!(
                "ChatGPT response exceeded {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            process_sse_data(&event_data, &mut state)?;
            event_data.clear();
        } else if let Some(data) = line.strip_prefix("data:") {
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(data.trim_start());
        }
    }
    if !state.completed {
        return Err(ChatGptError::Protocol(
            "ChatGPT response stream ended before response.completed".to_owned(),
        ));
    }

    let mut tool_calls = Vec::new();
    for item_id in state.call_order {
        let Some(call) = state.calls.remove(&item_id) else {
            continue;
        };
        if call.call_id.is_empty() || call.name.is_empty() {
            return Err(ChatGptError::Protocol(
                "ChatGPT emitted an incomplete function call".to_owned(),
            ));
        }
        serde_json::from_str::<Value>(&call.arguments).map_err(|source| {
            ChatGptError::Protocol(format!(
                "ChatGPT emitted invalid arguments for {}: {source}",
                call.name
            ))
        })?;
        tool_calls.push(ModelToolCall {
            id: call.call_id,
            kind: "function".to_owned(),
            function: ModelFunctionCall {
                name: call.name,
                arguments: call.arguments,
            },
        });
    }
    let content = (!state.text.trim().is_empty()).then_some(state.text);
    if content.is_none() && tool_calls.is_empty() {
        return Err(ChatGptError::Protocol(
            "ChatGPT returned neither text nor tool calls".to_owned(),
        ));
    }
    Ok(AssistantTurn {
        content,
        tool_calls,
        usage: state.usage,
        replay_items: state.replay_items,
    })
}

fn process_sse_data(data: &str, state: &mut StreamState) -> Result<(), ChatGptError> {
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return Ok(());
    }
    let event = serde_json::from_str::<Value>(data)
        .map_err(|source| ChatGptError::Protocol(format!("invalid SSE event: {source}")))?;
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "response.output_item.added" => {
            if let Some(item) = event.get("item") {
                remember_call(item, state);
            }
        }
        "response.output_text.delta" => {
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                state.text.push_str(delta);
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = event.get("item_id").and_then(Value::as_str);
            let delta = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(call) = pending_call_mut(state, item_id) {
                call.arguments.push_str(delta);
            }
        }
        "response.function_call_arguments.done" => {
            let item_id = event.get("item_id").and_then(Value::as_str);
            let arguments = event
                .get("arguments")
                .and_then(Value::as_str)
                .filter(|arguments| !arguments.is_empty());
            if let Some(arguments) = arguments
                && let Some(call) = pending_call_mut(state, item_id)
            {
                call.arguments = arguments.to_owned();
            }
        }
        "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                finish_item(item, state)?;
            }
        }
        "response.completed" => {
            let status = event
                .get("response")
                .and_then(|response| response.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("completed");
            if status != "completed" {
                return Err(ChatGptError::Protocol(format!(
                    "ChatGPT response finished with status {status}"
                )));
            }
            // Usage is accounting, not content: a malformed report is dropped rather than failing
            // a turn whose text and tool calls arrived intact.
            state.usage = event
                .get("response")
                .and_then(|response| response.get("usage"))
                .and_then(|usage| serde_json::from_value::<WireResponsesUsage>(usage.clone()).ok())
                .map(ModelUsage::from);
            state.completed = true;
        }
        "response.failed" | "response.incomplete" | "error" => {
            return Err(ChatGptError::Protocol(format_provider_error(&event)));
        }
        _ => {}
    }
    Ok(())
}

fn remember_call(item: &Value, state: &mut StreamState) {
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return;
    }
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if item_id.is_empty() {
        return;
    }
    if !state.calls.contains_key(&item_id) {
        state.call_order.push(item_id.clone());
    }
    let call = state.calls.entry(item_id.clone()).or_default();
    call.item_id = item_id;
    update_call(call, item);
}

fn update_call(call: &mut PendingCall, item: &Value) {
    if let Some(value) = item.get("call_id").and_then(Value::as_str) {
        call.call_id = value.to_owned();
    }
    if let Some(value) = item.get("name").and_then(Value::as_str) {
        call.name = value.to_owned();
    }
    if let Some(value) = item
        .get("arguments")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        call.arguments = value.to_owned();
    }
}

fn pending_call_mut<'a>(
    state: &'a mut StreamState,
    item_id: Option<&str>,
) -> Option<&'a mut PendingCall> {
    if let Some(item_id) = item_id {
        return state.calls.get_mut(item_id);
    }
    let item_id = state.call_order.last()?.clone();
    state.calls.get_mut(&item_id)
}

fn finish_item(item: &Value, state: &mut StreamState) -> Result<(), ChatGptError> {
    match item.get("type").and_then(Value::as_str) {
        Some("reasoning") => state.replay_items.push(item.clone()),
        Some("message") => {
            if state.text.is_empty() {
                state.text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<String>();
            }
            state.replay_items.push(item.clone());
        }
        Some("function_call") => {
            remember_call(item, state);
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let call = state.calls.get_mut(item_id).ok_or_else(|| {
                ChatGptError::Protocol("function call omitted item ID".to_owned())
            })?;
            update_call(call, item);
            if !call.replayed {
                let mut replay = item.clone();
                if let Some(object) = replay.as_object_mut() {
                    object.insert(
                        "arguments".to_owned(),
                        Value::String(call.arguments.clone()),
                    );
                }
                state.replay_items.push(replay);
                call.replayed = true;
            }
        }
        _ => {}
    }
    Ok(())
}

fn format_provider_error(event: &Value) -> String {
    sanitize_diagnostic(
        event
            .pointer("/error/message")
            .or_else(|| event.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("ChatGPT response failed"),
    )
}

fn read_error_body(response: http::Response<ureq::Body>) -> String {
    let mut body = response.into_parts().1.into_reader().take(16 * 1024);
    let mut text = String::new();
    let _ = body.read_to_string(&mut text);
    sanitize_diagnostic(&text)
}

fn sanitize_diagnostic(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

fn resolve_auth_path(explicit: Option<&Path>) -> Result<PathBuf, ChatGptError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = env::var_os("DEKOPON_CHATGPT_AUTH_FILE") {
        return Ok(PathBuf::from(path));
    }
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config)
            .join("dekopon")
            .join("chatgpt-auth.json"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("dekopon")
            .join("chatgpt-auth.json"));
    }
    if let Some(app_data) = env::var_os("APPDATA") {
        return Ok(PathBuf::from(app_data)
            .join("dekopon")
            .join("chatgpt-auth.json"));
    }
    Err(ChatGptError::Configuration(
        "could not determine credential path; set DEKOPON_CHATGPT_AUTH_FILE".to_owned(),
    ))
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
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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

fn unix_time() -> Result<u64, ChatGptError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| ChatGptError::Configuration("system clock is before Unix epoch".to_owned()))
}

#[derive(Debug, Error)]
enum ChatGptRequestError {
    #[error("ChatGPT authorization expired")]
    Unauthorized,
    #[error("ChatGPT request failed: {0}")]
    Transport(String),
    #[error("ChatGPT returned HTTP {status}: {detail}")]
    Status { status: u16, detail: String },
    #[error("invalid ChatGPT response: {0}")]
    Protocol(String),
}

/// Failure while authenticating or using a ChatGPT subscription.
#[derive(Debug, Error)]
pub enum ChatGptError {
    /// Configuration was invalid.
    #[error("invalid ChatGPT configuration: {0}")]
    Configuration(String),
    /// No Dekopon-owned login exists.
    #[error("not logged in to ChatGPT; run `dekopon auth chatgpt login` (expected {})", path.display())]
    NotLoggedIn {
        /// Expected credential path.
        path: PathBuf,
    },
    /// Reading the credential file failed.
    #[error("could not read ChatGPT credentials at {}", path.display())]
    ReadAuth {
        /// Credential path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: io::Error,
    },
    /// Parsing the credential file failed.
    #[error("could not parse ChatGPT credentials at {}", path.display())]
    ParseAuth {
        /// Credential path.
        path: PathBuf,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Serializing credentials failed.
    #[error("could not serialize ChatGPT credentials at {}", path.display())]
    SerializeAuth {
        /// Temporary credential path.
        path: PathBuf,
        /// JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// Writing credentials failed.
    #[error("could not write ChatGPT credentials at {}", path.display())]
    WriteAuth {
        /// Credential path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: io::Error,
    },
    /// Removing credentials failed.
    #[error("could not remove ChatGPT credentials at {}", path.display())]
    RemoveAuth {
        /// Credential path.
        path: PathBuf,
        /// Filesystem error.
        #[source]
        source: io::Error,
    },
    /// Writing interactive login output failed.
    #[error("could not write ChatGPT login instructions")]
    Output {
        /// Output error.
        #[source]
        source: io::Error,
    },
    /// An HTTPS request failed.
    #[error("ChatGPT authentication request failed: {0}")]
    Request(String),
    /// OpenAI returned malformed OAuth data.
    #[error("invalid ChatGPT authentication response: {0}")]
    Protocol(String),
    /// Login was rejected.
    #[error("ChatGPT login failed: {0}")]
    Login(String),
    /// Login took too long.
    #[error("ChatGPT device login timed out after 15 minutes")]
    LoginTimeout,
    /// Reading the streaming response failed.
    #[error("could not read ChatGPT response stream")]
    Stream {
        /// Stream error.
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{BufRead as _, BufReader, Read as _, Write as _},
        net::{TcpListener, TcpStream},
        path::Path,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use dekopon_core::Redacted;

    use super::{
        AUTH_VERSION, ChatGptCodexModel, ChatGptCredentials, ChatGptEndpoints, build_request_body,
        export_credentials, extract_account_id, load_credentials, login_with_endpoints, logout,
        parse_sse, save_credentials, status,
    };
    use crate::model::{ChatModel as _, CompletionOptions, ModelMessage, ModelTool};

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
    fn missing_credentials_point_to_the_operator_auth_command() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("missing-auth.json");
        let error = match ChatGptCodexModel::new("gpt-test", Some(&path), Duration::from_secs(1)) {
            Ok(_) => panic!("missing credentials must fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("dekopon auth chatgpt login"));
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

    #[test]
    fn builds_codex_responses_payload_with_replay_items() {
        let mut assistant = crate::model::AssistantTurn {
            content: None,
            tool_calls: Vec::new(),
            usage: None,
            replay_items: vec![json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "opaque"
            })],
        };
        assistant.tool_calls.push(crate::model::ModelToolCall {
            id: "call-1".to_owned(),
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
        let body = build_request_body(
            "gpt-test",
            &messages,
            &[ModelTool {
                name: "echo_echo".to_owned(),
                description: "Echo".to_owned(),
                parameters: json!({"type":"object"}),
            }],
            &CompletionOptions::default(),
        );

        assert_eq!(body["instructions"], "Be concise");
        assert_eq!(body["tools"][0]["name"], "echo_echo");
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][2]["type"], "function_call_output");
    }

    /// Serializes one request fragment so a comparison is over the bytes a provider's prefix cache
    /// would hash rather than over two parsed values that merely agree.
    ///
    /// Both sides always come from this same binary, which is what keeps this a relation between
    /// two computed values instead of a golden string: `serde_json`'s `preserve_order` feature
    /// reaches this crate through a dev-dependency, and under resolver 3 dev-dependency features
    /// unify into `cargo test` but not `cargo build`. Object key order is therefore deterministic
    /// within either binary — which is all prefix stability needs — but not necessarily the same
    /// order in both, so a literal expected body would fail for a reason that has nothing to do
    /// with the property under test.
    fn request_text(fragment: &Value) -> String {
        serde_json::to_string(fragment).expect("serialize request fragment")
    }

    /// The single scripting tool a real session offers, so the fixtures below grow the way
    /// `dekopon-agent`'s prompt loop actually grows a conversation.
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

    /// One assistant turn shaped the way the Codex transport reports it: encrypted reasoning and
    /// the function call as opaque replay items, plus the same call surfaced as a tool call.
    fn scripted_turn(turn: u32, call_id: &str, script: &str) -> crate::model::AssistantTurn {
        let arguments = json!({"script": script}).to_string();
        crate::model::AssistantTurn {
            content: None,
            tool_calls: vec![crate::model::ModelToolCall {
                id: call_id.to_owned(),
                kind: "function".to_owned(),
                function: crate::model::ModelFunctionCall {
                    name: "bash".to_owned(),
                    arguments: arguments.clone(),
                },
            }],
            usage: None,
            replay_items: vec![
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
        }
    }

    #[test]
    fn an_appended_turn_extends_the_request_input_and_leaves_its_prefix_untouched() {
        // Automatic prefix caching keys on the leading bytes of a request: it pays only when turn
        // N+1 is turn N with more appended and nothing at all rewritten. The prompt loop appends
        // and never edits, so this holds today by construction — the point of pinning it is that a
        // regression here is silent. No error, no wrong answer, just every turn of every session
        // paying full price for a prompt the provider already had.
        let tools = vec![bash_tool()];
        let mut messages = vec![
            ModelMessage::system("Be concise."),
            ModelMessage::user("how many files are in the repository?"),
        ];
        let mut bodies = vec![build_request_body(
            "gpt-test",
            &messages,
            &tools,
            &CompletionOptions::default(),
        )];
        for (turn, script) in [(1, "ls | wc -l"), (2, "ls -a | wc -l")] {
            let call_id = format!("call_{turn}");
            let assistant = scripted_turn(turn, &call_id, script);
            messages.push(crate::model::assistant_message(&assistant));
            messages.push(ModelMessage::tool(call_id.as_str(), "12\n"));
            bodies.push(build_request_body(
                "gpt-test",
                &messages,
                &tools,
                &CompletionOptions::default(),
            ));
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

    #[test]
    fn a_system_message_anywhere_in_history_rewrites_the_front_of_the_request() {
        // `build_request_body` hoists *every* system message into the top-level `instructions`
        // field, wherever it sits in the list, and emits nothing for it in `input`. A system
        // message injected mid-conversation therefore appends nothing and edits the very first
        // bytes of the request instead, voiding the whole cached prefix while `input` still looks
        // perfectly append-only — which is why the two input arrays below are identical and the
        // instructions are not. History must never carry a system message past the opening one,
        // and this test is where that constraint is recorded.
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

        let plain = build_request_body("gpt-test", &history, &tools, &CompletionOptions::default());
        let hoisted =
            build_request_body("gpt-test", &injected, &tools, &CompletionOptions::default());

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

    #[test]
    fn a_repeated_system_message_silently_doubles_the_instructions() {
        // Nothing deduplicates and nothing complains. A caller that re-seeds the system prompt onto
        // a conversation that already carries one sends the whole thing twice: double the
        // instruction tokens on every subsequent turn, a prefix that no longer matches anything
        // cached, and a model reading its own instructions in stereo.
        let system = "Be concise.";
        let messages = vec![
            ModelMessage::system(system),
            ModelMessage::user("how many files are in the repository?"),
            ModelMessage::system(system),
        ];

        let body = build_request_body("gpt-test", &messages, &[], &CompletionOptions::default());

        assert_eq!(body["instructions"], format!("{system}\n\n{system}"));
        assert_eq!(
            body["input"].as_array().expect("input array").len(),
            1,
            "neither system message reaches input, so the duplication is invisible there"
        );
    }

    #[test]
    fn codex_requests_never_ask_the_provider_to_retain_the_conversation() {
        // `store: false` is a data-retention decision rather than a tuning knob. Nothing about a
        // session is kept server-side, which is precisely why the transport has to carry encrypted
        // reasoning back itself through `replay_items` and ask for it with `include`. Flipping the
        // literal would move conversation content into someone else's storage while every test
        // here kept passing, so the assertion is written to be deleted deliberately rather than
        // edged past.
        let assistant = scripted_turn(1, "call_1", "ls | wc -l");
        let opening = build_request_body(
            "gpt-test",
            &[ModelMessage::user("hello")],
            &[],
            &CompletionOptions::default(),
        );
        let resumed = build_request_body(
            "gpt-test",
            &[
                ModelMessage::user("how many files are in the repository?"),
                crate::model::assistant_message(&assistant),
                ModelMessage::tool("call_1", "12\n"),
            ],
            &[bash_tool()],
            &CompletionOptions::default(),
        );

        for body in [&opening, &resumed] {
            assert_eq!(body["store"], false);
            assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        }
    }

    /// The conversation the cache-key tests below build requests from, kept in one place so the
    /// only difference between the bodies they compare is the options value.
    fn cached_conversation() -> Vec<ModelMessage> {
        let assistant = scripted_turn(1, "call_1", "ls | wc -l");
        vec![
            ModelMessage::system("Be concise."),
            ModelMessage::user("how many files are in the repository?"),
            crate::model::assistant_message(&assistant),
            ModelMessage::tool("call_1", "12\n"),
        ]
    }

    #[test]
    fn a_request_without_a_cache_key_carries_no_cache_field_at_all() {
        // No caller supplies a key yet, so this is the body every real request still has: an
        // absent key must serialize away completely rather than as a null or an empty string.
        // Anything else would be a wire change shipped by a feature nobody has switched on.
        let messages = cached_conversation();
        let tools = vec![bash_tool()];
        let plain =
            build_request_body("gpt-test", &messages, &tools, &CompletionOptions::default());

        assert!(
            plain.get("prompt_cache_key").is_none(),
            "a keyless request grew a cache field"
        );
        assert!(
            !request_text(&plain).contains("prompt_cache_key"),
            "the field name reached the wire without a key to carry"
        );

        // A caller deriving a key from an empty conversation ID must land on the same bytes rather
        // than routing every such session into one shared empty lane.
        let blank = build_request_body(
            "gpt-test",
            &messages,
            &tools,
            &CompletionOptions::default().with_prompt_cache_key("   "),
        );
        assert_eq!(request_text(&blank), request_text(&plain));
    }

    #[test]
    fn a_cache_key_adds_one_field_and_disturbs_nothing_else() {
        // The key is worth nothing if setting it edits the very prefix it is meant to match. Every
        // field the previous request had must survive unchanged; the key may only be added.
        let messages = cached_conversation();
        let tools = vec![bash_tool()];
        let plain =
            build_request_body("gpt-test", &messages, &tools, &CompletionOptions::default());
        let keyed = build_request_body(
            "gpt-test",
            &messages,
            &tools,
            &CompletionOptions::default().with_prompt_cache_key("session-7"),
        );

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

    #[test]
    fn the_codex_transport_sends_a_cache_key_only_when_a_caller_supplies_one() {
        // Proves the plumbing reaches the socket and that plain `complete` still does not: the
        // trait's keyless entry point delegates with default options, so today's callers send
        // exactly what they sent before.
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
        let model = ChatGptCodexModel::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
        )
        .expect("model client");
        let messages = vec![ModelMessage::user("hello")];

        model.complete(&messages, &[]).expect("keyless turn");
        model
            .complete_with(
                &messages,
                &[],
                &CompletionOptions::default().with_prompt_cache_key("session-7"),
            )
            .expect("keyed turn");

        // Substrings rather than a body comparison: the exact spacing is the HTTP client's
        // choice, and what matters here is only which request carried the key.
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

        let turn = parse_sse(stream.as_bytes()).expect("valid response stream");

        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.name, "echo_echo");
        assert_eq!(
            turn.tool_calls[0].function.arguments,
            r#"{"message":"hello"}"#
        );
        assert_eq!(turn.replay_items[0]["type"], "reasoning");
        assert_eq!(turn.replay_items[1]["arguments"], r#"{"message":"hello"}"#);
        assert_eq!(
            turn.usage,
            Some(crate::model::ModelUsage {
                input_tokens: Some(120),
                cached_input_tokens: Some(100),
                output_tokens: Some(30),
                reasoning_output_tokens: Some(7),
                total_tokens: Some(150),
            })
        );
    }

    #[test]
    fn a_usage_free_completion_leaves_usage_absent() {
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );

        let turn = parse_sse(stream.as_bytes()).expect("valid response stream");

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

    #[test]
    fn subscription_model_replays_reasoning_and_correlates_tool_results() {
        let first = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"opaque\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo_echo\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"message\\\":\\\"hello\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"echo_echo\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );
        let second = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Echoed hello.\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
        );
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
        let model = ChatGptCodexModel::with_endpoints(
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

        let tool_turn = model.complete(&messages, &tools).expect("tool turn");
        assert_eq!(tool_turn.tool_calls[0].id, "call_1");
        messages.push(crate::model::assistant_message(&tool_turn));
        messages.push(ModelMessage::tool("call_1", r#"{"message":"hello"}"#));
        let answer = model.complete(&messages, &tools).expect("answer turn");

        assert_eq!(answer.content.as_deref(), Some("Echoed hello."));
        let requests = server.requests.lock().expect("request lock");
        assert!(requests[1].contains("opaque"));
        assert!(requests[1].contains("function_call_output"));
        assert!(requests[1].contains("call_1"));
    }

    #[test]
    fn subscription_model_refreshes_expired_credentials_before_inference() {
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
        let model = ChatGptCodexModel::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
        )
        .expect("model client");

        let turn = model
            .complete(&[ModelMessage::user("hello")], &[])
            .expect("model turn");

        assert_eq!(turn.content.as_deref(), Some("refreshed"));
        let credentials = load_credentials(&path).expect("refreshed credentials persisted");
        assert_eq!(credentials.account_id, "acct-refreshed");
        assert_eq!(credentials.refresh.expose(), "refresh-new");
        let requests = server.requests.lock().expect("request lock");
        assert!(requests[0].contains("grant_type=refresh_token"));
        assert!(requests[1].contains("chatgpt-account-id: acct-refreshed"));
    }

    #[test]
    fn subscription_model_sends_required_headers_and_decodes_text() {
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
        let model = ChatGptCodexModel::with_endpoints(
            "gpt-test",
            Some(&path),
            Duration::from_secs(2),
            endpoints,
        )
        .expect("model client");

        let turn = model
            .complete(&[ModelMessage::user("hello")], &[])
            .expect("model turn");

        assert_eq!(turn.content.as_deref(), Some("hello"));
        let request = server.requests.lock().expect("request lock")[0].clone();
        assert!(request.contains("authorization: Bearer header."));
        assert!(request.contains("chatgpt-account-id: acct-test"));
        assert!(request.contains("originator: dekopon"));
    }

    struct MockResponse {
        status: u16,
        content_type: &'static str,
        body: String,
    }

    impl MockResponse {
        fn json(body: Value) -> Self {
            Self {
                status: 200,
                content_type: "application/json",
                body: body.to_string(),
            }
        }

        fn sse(body: &str) -> Self {
            Self {
                status: 200,
                content_type: "text/event-stream",
                body: body.to_owned(),
            }
        }
    }

    struct MockServer {
        address: String,
        requests: Arc<Mutex<Vec<String>>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl MockServer {
        fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock endpoint");
            let address = listener
                .local_addr()
                .expect("mock endpoint address")
                .to_string();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_requests = Arc::clone(&requests);
            let handle = thread::spawn(move || {
                for response in responses {
                    let (mut stream, _) = listener.accept().expect("accept request");
                    let request = read_request(&mut stream);
                    thread_requests.lock().expect("request lock").push(request);
                    write!(
                        stream,
                        "HTTP/1.1 {} OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response.status,
                        response.content_type,
                        response.body.len(),
                        response.body
                    )
                    .expect("write response");
                }
            });
            Self {
                address,
                requests,
                handle: Some(handle),
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.address)
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            if let Some(handle) = self.handle.take() {
                handle.join().expect("mock server thread");
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut reader = BufReader::new(stream.try_clone().expect("clone request stream"));
        let mut request = String::new();
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request line");
            request.push_str(&line);
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().expect("content length");
            }
            if line == "\r\n" {
                break;
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).expect("read request body");
        request.push_str(&String::from_utf8(body).expect("UTF-8 request"));
        request
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

    /// An exported document must be byte-identical to what a login wrote, so a file seeded from it
    /// is indistinguishable from a locally created one.
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

    /// The export wrapper must not quietly become a new way to print a credential: only the named
    /// accessor exposes it.
    #[test]
    fn export_debug_rendering_stays_redacted() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        export_fixture(&path);

        let export = export_credentials(Some(&path)).expect("export credentials");

        assert!(!format!("{export:?}").contains("refresh-secret"));
        assert!(format!("{export:?}").contains("REDACTED"));
    }

    /// Exporting nothing must fail loudly rather than emit an empty document a seeding step would
    /// happily store.
    #[test]
    fn export_without_a_login_fails_with_the_login_instruction() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("missing-auth.json");

        let error = export_credentials(Some(&path)).expect_err("missing credentials must fail");

        assert!(error.to_string().contains("dekopon auth chatgpt login"));
    }

    /// A credential file that parses but carries empty tokens must fail too; a half-formed export
    /// is the failure mode that survives into a cluster.
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

    /// Malformed JSON must name the file rather than produce a document.
    #[test]
    fn export_rejects_a_malformed_credential_file() {
        let temp = TempDir::new().expect("temporary directory");
        let path = temp.path().join("auth.json");
        fs::write(&path, "{ not json").expect("write malformed fixture");

        let error = export_credentials(Some(&path)).expect_err("malformed credentials must fail");

        assert!(error.to_string().contains("could not parse"), "{error}");
    }

    #[allow(dead_code)]
    fn _assert_private_path(_path: &Path) {}
}
