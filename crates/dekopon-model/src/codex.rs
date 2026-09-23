//! Async Codex Responses generation; account login and refresh stay in `chatgpt`.

use crate::{
    chatgpt::{ChatGptError, CredentialFile, ResolvedCredential, resolve_auth_path},
    control::TurnControl,
    diagnostic::DiagnosticSecrets,
    error::{
        AuthError, FailurePhase, InferenceError, ProtocolFailure, ProviderFailure, RequestError,
        TransportFailure,
    },
    http::{InferenceHttp, Progress, record_phase},
    inference::{GenerateRequest, InferenceModel},
    model::{
        AssistantTurn, ClientIdentity, CompletionOptions, ContentPart, DataUrl, JSON_CONTENT_TYPE,
        ModelFunctionCall, ModelMessage, ModelTool, ModelToolCall, ModelUsage, compact_json_body,
        sanitize_diagnostic,
    },
    sse::{SseEvent, decode_transcript},
    stream::{ModelText, TurnEvent},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    ops::ControlFlow,
    path::Path,
    sync::Arc,
    time::Duration,
};

pub(crate) const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// One pooled Codex client with its own credential snapshot and continuation identity.
pub struct CodexClient {
    name: String,
    model: String,
    identity: ClientIdentity,
    credential: Arc<CredentialFile>,
    http: InferenceHttp,
    endpoint: String,
    loopback: bool,
}

impl CodexClient {
    /// Loads the credential on the calling blocking thread; generation is asynchronous.
    pub fn new(
        model: impl Into<String>,
        auth_path: Option<&Path>,
        timeout: Duration,
    ) -> Result<Self, InferenceError> {
        let model = model.into();
        Self::validate(&model, timeout)?;
        let path = resolve_auth_path(auth_path).map_err(AuthError::Credential)?;
        let credential = CredentialFile::open(&path, timeout).map_err(AuthError::Credential)?;
        Self::configured(model, credential, timeout, RESPONSES_URL.to_owned())
    }

    fn validate(model: &str, timeout: Duration) -> Result<(), InferenceError> {
        if model.trim().is_empty() {
            return Err(RequestError::EmptyModel.into());
        }
        if timeout.is_zero() {
            return Err(RequestError::ZeroTimeout.into());
        }
        Ok(())
    }

    fn configured(
        model: String,
        credential: CredentialFile,
        timeout: Duration,
        endpoint: String,
    ) -> Result<Self, InferenceError> {
        Ok(Self {
            name: model.clone(),
            model,
            identity: ClientIdentity::new(),
            credential: Arc::new(credential),
            http: InferenceHttp::new(timeout)?,
            endpoint,
            loopback: false,
        })
    }

    /// Sets the operator's configured name for local telemetry, never an upstream header.
    #[must_use]
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }

    /// Overrides generation for an offline fixture at literal `127.0.0.1` or `::1` over HTTP.
    ///
    /// Credentials are never refreshed: a `401` or a token inside its refresh margin fails with
    /// [`InferenceError::Authentication`]. Existing continuations are invalidated.
    pub fn with_loopback_endpoint(mut self, endpoint: &str) -> Result<Self, InferenceError> {
        self.endpoint = crate::loopback::endpoint(endpoint)?;
        self.loopback = true;
        self.identity = ClientIdentity::new();
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn with_endpoints(
        model: impl Into<String>,
        auth_path: Option<&Path>,
        timeout: Duration,
        endpoints: crate::chatgpt::ChatGptEndpoints,
    ) -> Result<Self, InferenceError> {
        let model = model.into();
        Self::validate(&model, timeout)?;
        let path = resolve_auth_path(auth_path).map_err(AuthError::Credential)?;
        let endpoint = endpoints.responses.clone();
        let credential = CredentialFile::with_endpoints(&path, timeout, endpoints)
            .map_err(AuthError::Credential)?;
        Self::configured(model, credential, timeout, endpoint)
    }

    async fn credential(
        &self,
        mode: RefreshMode,
        control: &TurnControl,
    ) -> Result<ResolvedCredential, InferenceError> {
        control
            .check()
            .inspect_err(|_| record_phase(FailurePhase::BeforeSend))?;
        if self.loopback {
            return self
                .credential
                .unrefreshed()
                .map_err(|source| AuthError::Credential(source).into());
        }
        let credential = Arc::clone(&self.credential);
        let span = tracing::Span::current();
        control
            .run(tokio::task::spawn_blocking(move || {
                span.in_scope(|| match mode {
                    RefreshMode::IfNeeded => credential.current(),
                    RefreshMode::Forced(rejected) => credential.force_refresh(&rejected),
                })
            }))
            .await
            .inspect_err(|_| record_phase(FailurePhase::BeforeSend))?
            .map_err(TransportFailure::Blocking)?
            .map_err(credential_failure)
    }

    async fn exchange(
        &self,
        request: GenerateRequest<'_>,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        let mut progress = Progress::new();
        for message in request.messages {
            if let ModelMessage::Assistant { turn } = message
                && !turn.accepts_client(&self.identity)
            {
                record_phase(FailurePhase::BeforeSend);
                return Err(ProtocolFailure::ContinuationMismatch.into());
            }
        }
        let credentials = self.credential(RefreshMode::IfNeeded, control).await?;
        let body = control
            .run(build_request_body(
                &self.model,
                request.messages,
                request.tools,
                request.options,
            ))
            .await
            .inspect_err(|_| record_phase(FailurePhase::BeforeSend))??;
        let encoded = compact_json_body(&body).map_err(TransportFailure::Encoding)?;
        drop(body);
        let builder = self
            .http
            .post(&self.endpoint)
            .header("originator", "dekopon")
            .header(
                "user-agent",
                format!("dekopon/{}", env!("CARGO_PKG_VERSION")),
            )
            .header("openai-beta", "responses=experimental")
            .header("accept", "text/event-stream")
            .header("content-type", JSON_CONTENT_TYPE)
            .body(encoded);
        let first = builder.try_clone().ok_or(RequestError::NonReplayableBody)?;
        let mut secrets = DiagnosticSecrets::new(Some(&credentials.access));
        let refreshed;
        let mut response = self
            .http
            .send(
                first
                    .bearer_auth(credentials.access.expose())
                    .header("chatgpt-account-id", &credentials.account_id),
                control,
                secrets,
            )
            .await?;
        if response.status() == 401 && !self.loopback {
            // No body byte is consumed on this one permitted resend; the deadline is unchanged.
            drop(response);
            refreshed = self
                .credential(RefreshMode::Forced(credentials.access.clone()), control)
                .await?;
            secrets =
                DiagnosticSecrets::new(Some(&refreshed.access)).with_previous(&credentials.access);
            response = self
                .http
                .send(
                    builder
                        .bearer_auth(refreshed.access.expose())
                        .header("chatgpt-account-id", &refreshed.account_id),
                    control,
                    secrets,
                )
                .await?;
        }
        let response = response.check(control, secrets).await?;
        let mut reducer = CodexReducer {
            secrets,
            reported_model: request
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    ModelMessage::Assistant { turn } => turn.codex_reported_model(),
                    _ => None,
                })
                .map(str::to_owned),
            ..CodexReducer::default()
        };
        response
            .sse(control, &mut |event| {
                progress.event();
                let SseEvent::Data(data) = event else {
                    return Ok(ControlFlow::Break(()));
                };
                reducer.apply(data, &mut |event| progress.observe(event, observe))
            })
            .await?;
        control
            .check()
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        let turn = reducer
            .finish(&self.identity)
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        control
            .check()
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        tracing::Span::current().record("output.partial", false);
        Ok(turn)
    }
}

impl InferenceModel for CodexClient {
    async fn generate<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        self.http
            .generation(
                &self.name,
                &self.model,
                "chatgpt-subscription",
                "codex-responses",
                self.exchange(request, observe, control),
            )
            .await
    }
}

enum RefreshMode {
    IfNeeded,
    Forced(dekopon_core::Redacted<String>),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResponseStatus {
    Queued,
    InProgress,
    Completed,
    Incomplete,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Deserialize)]
enum EventKind {
    #[serde(rename = "response.output_item.added")]
    ItemAdded,
    #[serde(rename = "response.output_item.done")]
    ItemDone,
    #[serde(rename = "response.output_text.delta")]
    TextDelta,
    #[serde(rename = "response.function_call_arguments.delta")]
    ArgumentsDelta,
    #[serde(rename = "response.function_call_arguments.done")]
    ArgumentsDone,
    #[serde(rename = "response.completed")]
    Completed,
    #[serde(rename = "response.failed")]
    Failed,
    #[serde(rename = "response.incomplete")]
    Incomplete,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "response.created")]
    Created,
    #[serde(rename = "response.in_progress")]
    InProgress,
    #[serde(rename = "response.queued")]
    Queued,
    #[serde(rename = "response.content_part.added")]
    ContentAdded,
    #[serde(rename = "response.content_part.done")]
    ContentDone,
    #[serde(rename = "response.output_text.done")]
    TextDone,
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningPartAdded,
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningPartDone,
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningDelta,
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningDone,
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta,
    #[serde(rename = "response.reasoning_text.done")]
    ReasoningTextDone,
}

#[derive(Deserialize)]
struct WireEvent {
    item: Option<Value>,
    delta: Option<String>,
    item_id: Option<String>,
    arguments: Option<String>,
    response: Option<WireResponse>,
    error: Option<WireFailure>,
    message: Option<String>,
    code: Option<String>,
}
#[derive(Deserialize)]
struct WireResponse {
    status: Option<ResponseStatus>,
    model: Option<String>,
    usage: Option<WireResponsesUsage>,
    error: Option<WireFailure>,
    #[serde(default)]
    output: Vec<Value>,
}
#[derive(Deserialize)]
struct WireFailure {
    code: Option<String>,
    message: Option<String>,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemKind {
    Message,
    Reasoning,
    FunctionCall,
}

#[derive(Deserialize)]
struct WireItem<'a> {
    #[serde(rename = "type")]
    kind: ItemKind,
    #[serde(default)]
    id: &'a str,
    #[serde(default)]
    call_id: &'a str,
    #[serde(default)]
    name: &'a str,
    #[serde(default)]
    arguments: &'a str,
    #[serde(default, borrow)]
    content: Option<Vec<WireText<'a>>>,
}
#[derive(Deserialize)]
struct WireText<'a> {
    #[serde(default)]
    text: &'a str,
}

#[derive(Default)]
enum Terminal {
    #[default]
    Streaming,
    Completed,
}
#[derive(Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
enum TextSource {
    #[default]
    Items,
    Deltas,
}

/// A socket-independent reducer: offline fixtures and the async body use exactly this code.
#[derive(Default)]
pub(crate) struct CodexReducer<'a> {
    secrets: DiagnosticSecrets<'a>,
    text: String,
    text_source: TextSource,
    native_items: Vec<Value>,
    native_positions: HashMap<String, usize>,
    calls: BTreeMap<String, PendingCall>,
    call_order: Vec<String>,
    terminal: Terminal,
    usage: Option<ModelUsage>,
    reported_model: Option<String>,
}

impl CodexReducer<'_> {
    pub(crate) fn apply(
        &mut self,
        data: &str,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<ControlFlow<()>, InferenceError> {
        let event: Value =
            serde_json::from_str(data).map_err(|error| self.secrets.decode_failure(error))?;
        let kind = event
            .get("type")
            .and_then(|kind| EventKind::deserialize(kind).ok());
        let failed = matches!(
            kind,
            Some(EventKind::Failed | EventKind::Incomplete | EventKind::Error)
        ) || [event.get("error"), event.pointer("/response/error")]
            .into_iter()
            .flatten()
            .any(|error| !error.is_null());
        if kind.is_none() && !failed {
            return Ok(ControlFlow::Continue(()));
        }
        let event =
            WireEvent::deserialize(event).map_err(|error| self.secrets.decode_failure(error))?;
        if let Some(model) = event
            .response
            .as_ref()
            .and_then(|response| response.model.as_deref())
        {
            let model = self.secrets.sanitize(model);
            if self
                .reported_model
                .as_deref()
                .is_some_and(|prior| prior != model)
            {
                return Err(ProtocolFailure::ContinuationMismatch.into());
            }
            self.reported_model = Some(model);
            tracing::Span::current().record("model.returned", self.reported_model.as_deref());
        }
        if failed {
            let detail = event
                .error
                .or_else(|| event.response.and_then(|response| response.error));
            let message = detail
                .as_ref()
                .and_then(|error| error.message.as_deref())
                .or(event.message.as_deref())
                .unwrap_or("Codex response failed or was incomplete");
            let mut failure = ProviderFailure::new(200, &self.secrets.sanitize(message));
            failure.code = detail
                .and_then(|error| error.code)
                .or(event.code)
                .as_deref()
                .map(|value| self.secrets.sanitize(value));
            return Err(failure.into());
        }
        let Some(kind) = kind else {
            return Ok(ControlFlow::Continue(()));
        };
        match kind {
            EventKind::ItemAdded => {
                let item = event
                    .item
                    .ok_or_else(|| invalid_event("output item missing"))?;
                let Some(item) = self.parse_item(&item)? else {
                    return Ok(ControlFlow::Continue(()));
                };
                let index = self.call_order.len();
                self.remember(&item)?;
                if self.call_order.len() > index
                    && observe(TurnEvent::ToolCallStarted {
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                    })
                    .is_break()
                {
                    return Err(InferenceError::Cancelled);
                }
            }
            EventKind::ItemDone => self.finish_item(
                event
                    .item
                    .ok_or_else(|| invalid_event("output item missing"))?,
            )?,
            EventKind::TextDelta => {
                let delta = event
                    .delta
                    .ok_or_else(|| invalid_event("text delta missing"))?;
                if !delta.is_empty() {
                    self.text_source = TextSource::Deltas;
                    self.text.push_str(&delta);
                    if observe(TurnEvent::TextDelta(ModelText::from_model(delta))).is_break() {
                        return Err(InferenceError::Cancelled);
                    }
                }
            }
            EventKind::ArgumentsDelta | EventKind::ArgumentsDone => {
                let item_id = event
                    .item_id
                    .as_deref()
                    .or_else(|| self.call_order.last().map(String::as_str))
                    .ok_or_else(|| invalid_event("argument event without a call"))?;
                let call = self
                    .calls
                    .get_mut(item_id)
                    .ok_or_else(|| invalid_event("argument event for an unknown call"))?;
                match kind {
                    EventKind::ArgumentsDelta => call.arguments.push_str(
                        event
                            .delta
                            .as_deref()
                            .ok_or_else(|| invalid_event("argument delta missing"))?,
                    ),
                    EventKind::ArgumentsDone => {
                        if let Some(arguments) = event.arguments.filter(|value| !value.is_empty()) {
                            call.arguments = arguments;
                        }
                    }
                    _ => {}
                }
            }
            EventKind::Completed => {
                if let Some(response) = event.response {
                    if response
                        .status
                        .is_some_and(|status| !matches!(status, ResponseStatus::Completed))
                    {
                        return Err(invalid_event("terminal response was not completed"));
                    }
                    self.usage = response.usage.map(ModelUsage::from);
                    let mut unindexed = self.native_items.len() - self.native_positions.len();
                    for item in response.output {
                        let received = match item.get("id").and_then(Value::as_str) {
                            Some(id) => self.native_positions.contains_key(id),
                            None if unindexed > 0 => {
                                unindexed -= 1;
                                true
                            }
                            None => false,
                        };
                        if !received {
                            self.finish_item(item)?;
                        }
                    }
                }
                self.terminal = Terminal::Completed;
                tracing::Span::current().record("finish.reason", "completed");
                return Ok(ControlFlow::Break(()));
            }
            EventKind::Created
            | EventKind::InProgress
            | EventKind::Queued
            | EventKind::ContentAdded
            | EventKind::ContentDone
            | EventKind::TextDone
            | EventKind::ReasoningPartAdded
            | EventKind::ReasoningPartDone
            | EventKind::ReasoningDelta
            | EventKind::ReasoningDone
            | EventKind::ReasoningTextDelta
            | EventKind::ReasoningTextDone => {}
            EventKind::Failed | EventKind::Incomplete | EventKind::Error => {
                return Err(invalid_event("failed terminal response"));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn remember(&mut self, item: &WireItem<'_>) -> Result<(), InferenceError> {
        if !matches!(item.kind, ItemKind::FunctionCall) {
            return Ok(());
        }
        if item.id.is_empty() {
            return Err(invalid_event("function call omitted item ID"));
        }
        if !self.calls.contains_key(item.id) {
            self.call_order.push(item.id.to_owned());
        }
        let call = self.calls.entry(item.id.to_owned()).or_default();
        if !item.call_id.is_empty() {
            call.call_id = item.call_id.to_owned();
        }
        if !item.name.is_empty() {
            call.name = item.name.to_owned();
        }
        if !item.arguments.is_empty() {
            call.arguments = item.arguments.to_owned();
        }
        Ok(())
    }

    /// Items of unknown or missing type are ignored before their shared fields are parsed.
    fn parse_item<'v>(&self, value: &'v Value) -> Result<Option<WireItem<'v>>, InferenceError> {
        if value
            .get("type")
            .and_then(|kind| ItemKind::deserialize(kind).ok())
            .is_none()
        {
            return Ok(None);
        }
        WireItem::deserialize(value)
            .map(Some)
            .map_err(|error| self.secrets.decode_failure(error).into())
    }

    fn finish_item(&mut self, mut value: Value) -> Result<(), InferenceError> {
        let Some(item) = self.parse_item(&value)? else {
            return Ok(());
        };
        self.remember(&item)?;
        let position = value
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| self.native_positions.get(id))
            .copied();
        if matches!(item.kind, ItemKind::Message)
            && matches!(self.text_source, TextSource::Items)
            && position.is_none()
        {
            for part in item.content.iter().flatten() {
                self.text.push_str(part.text);
            }
        }
        if matches!(item.kind, ItemKind::FunctionCall) {
            let arguments = self
                .calls
                .get(item.id)
                .ok_or_else(|| invalid_event("completed call missing"))?
                .arguments
                .clone();
            if let Some(object) = value.as_object_mut() {
                object.insert("arguments".to_owned(), Value::String(arguments));
            }
        }
        if let Some(position) = position {
            let stored = self
                .native_items
                .get_mut(position)
                .and_then(Value::as_object_mut)
                .ok_or_else(|| invalid_event("native item must be an object"))?;
            if let Some(final_fields) = value.as_object_mut() {
                stored.append(final_fields);
            }
        } else {
            if let Some(id) = value.get("id").and_then(Value::as_str) {
                self.native_positions
                    .insert(id.to_owned(), self.native_items.len());
            }
            self.native_items.push(value);
        }
        Ok(())
    }

    pub(crate) fn finish(
        mut self,
        identity: &ClientIdentity,
    ) -> Result<AssistantTurn, InferenceError> {
        if !matches!(self.terminal, Terminal::Completed) {
            return Err(ProtocolFailure::MissingTerminal.into());
        }
        let mut calls = Vec::with_capacity(self.call_order.len());
        for id in self.call_order {
            let call = self
                .calls
                .remove(&id)
                .ok_or_else(|| invalid_event("completed call missing"))?;
            if call.call_id.is_empty() || call.name.is_empty() {
                return Err(invalid_event("incomplete function call"));
            }
            let arguments: Value = serde_json::from_str(&call.arguments)
                .map_err(|error| self.secrets.decode_failure(error))?;
            if !arguments.is_object() {
                return Err(ProtocolFailure::InvalidToolArguments {
                    name: self.secrets.sanitize(&call.name),
                }
                .into());
            }
            calls.push(ModelToolCall {
                id: call.call_id.into(),
                kind: "function".to_owned(),
                function: ModelFunctionCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            });
        }
        let content = (!self.text.trim().is_empty()).then_some(self.text);
        if content.is_none() && calls.is_empty() {
            return Err(invalid_event("response contained neither text nor calls"));
        }
        Ok(
            AssistantTurn::new(content, calls, self.usage).with_codex_continuation(
                self.native_items,
                identity.clone(),
                self.reported_model,
            ),
        )
    }
}

fn invalid_event(message: &str) -> InferenceError {
    ProtocolFailure::InvalidEvent(sanitize_diagnostic(message)).into()
}

pub(crate) fn replay_transcript(
    body: &str,
    observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
) -> Result<AssistantTurn, InferenceError> {
    let mut reducer = CodexReducer::default();
    decode_transcript(body, &mut |event| match event {
        SseEvent::Data(data) => reducer.apply(data, observe),
        SseEvent::Done => Ok(ControlFlow::Break(())),
    })?;
    reducer.finish(&ClientIdentity::new())
}
fn credential_failure(source: ChatGptError) -> InferenceError {
    match source {
        source @ ChatGptError::TokenRefused {
            status: 401 | 403, ..
        } => AuthError::Credential(source).into(),
        source @ ChatGptError::TokenRefused { status, .. } => {
            let failure = ProviderFailure::credential(status, source);
            if status == 429 {
                crate::error::RateLimitError(failure).into()
            } else {
                failure.into()
            }
        }
        source => TransportFailure::Credential(source).into(),
    }
}

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    store: bool,
    stream: bool,
    instructions: String,
    input: Vec<ResponsesItem<'a>>,
    tools: Vec<ResponsesTool<'a>>,
    tool_choice: &'static str,
    parallel_tool_calls: bool,
    include: [&'static str; 1],
    text: ResponsesText,
    /// Serialized only when a key exists, so a keyless request carries no such field at all.
    /// `prompt_cache_key` routes toward a warm prefix and authorizes nothing; the conversation
    /// itself is already in `input` either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct ResponsesText {
    verbosity: &'static str,
}

#[derive(Debug, Serialize)]
struct ResponsesTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

/// One entry of the `input` array.
///
/// Untagged because a replayed item is whatever the API sent last turn and already carries its own
/// `type`; the rest name theirs.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ResponsesItem<'a> {
    Replay(&'a Value),
    Message(ResponsesMessage<'a>),
    FunctionCall(ResponsesFunctionCall<'a>),
    FunctionCallOutput(ResponsesFunctionCallOutput<'a>),
}

#[derive(Debug, Serialize)]
struct ResponsesMessage<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    role: &'static str,
    content: Vec<ResponsesContent<'a>>,
}

#[derive(Debug, Serialize)]
struct ResponsesFunctionCall<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: &'a str,
    name: &'a str,
    arguments: &'a str,
}

#[derive(Debug, Serialize)]
struct ResponsesFunctionCallOutput<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    call_id: &'a str,
    output: &'a str,
}

/// One part of a message's `content` array.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ResponsesContent<'a> {
    #[serde(rename = "input_text")]
    InputText { text: Cow<'a, str> },
    #[serde(rename = "input_image")]
    InputImage { image_url: DataUrl<'a> },
    #[serde(rename = "input_file")]
    InputFile {
        filename: &'a str,
        file_data: DataUrl<'a>,
    },
    #[serde(rename = "output_text")]
    OutputText {
        text: &'a str,
        /// Always empty, and always present: the API requires the key on an assistant message.
        annotations: [Value; 0],
    },
}

/// The `content` array for one user message, text-only or multimodal.
///
/// The Responses API has taken an array here since before attachments existed, which is why this
/// transport needs one function rather than the wire-message type the chat-completions path grew.
async fn responses_content(
    message: &ModelMessage,
) -> Result<Vec<ResponsesContent<'_>>, InferenceError> {
    let Some(parts) = message.parts() else {
        return Ok(vec![ResponsesContent::InputText {
            text: message.content().unwrap_or_default().into(),
        }]);
    };
    let mut content = Vec::with_capacity(parts.len());
    for part in parts {
        let bytes = match part {
            ContentPart::Image { data, .. } | ContentPart::File { data, .. } => {
                let reference = data.clone();
                let span = tracing::Span::current();
                match tokio::task::spawn_blocking(move || span.in_scope(|| reference.read()))
                    .await
                    .map_err(TransportFailure::Blocking)?
                {
                    Ok(bytes) => Some(bytes),
                    Err(
                        crate::asset::BlobError::Reclaimed | crate::asset::BlobError::Unauthorized,
                    ) => {
                        content.push(ResponsesContent::InputText {
                            text: data.release_notice().into(),
                        });
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            ContentPart::Text(_) => None,
        };
        content.push(match part {
            ContentPart::Text(text) => ResponsesContent::InputText { text: text.into() },
            ContentPart::Image { mime, .. } => ResponsesContent::InputImage {
                image_url: DataUrl::new(mime, bytes.unwrap_or_default()),
            },
            ContentPart::File { name, mime, .. } => ResponsesContent::InputFile {
                filename: name,
                file_data: DataUrl::new(mime, bytes.unwrap_or_default()),
            },
        });
    }
    Ok(content)
}

async fn build_request_body<'a>(
    model: &'a str,
    messages: &'a [ModelMessage],
    tools: &'a [ModelTool],
    options: &'a CompletionOptions,
) -> Result<ResponsesRequest<'a>, InferenceError> {
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
        match message {
            ModelMessage::System { .. } => {}
            ModelMessage::User { .. } => input.push(ResponsesItem::Message(ResponsesMessage {
                kind: "message",
                role: "user",
                content: responses_content(message).await?,
            })),
            ModelMessage::Assistant { turn } => {
                if let Some(items) = turn.codex_items().filter(|items| !items.is_empty()) {
                    input.extend(items.iter().map(ResponsesItem::Replay));
                    continue;
                }
                if let Some(content) = message.content().filter(|content| !content.is_empty()) {
                    input.push(ResponsesItem::Message(ResponsesMessage {
                        kind: "message",
                        role: "assistant",
                        content: vec![ResponsesContent::OutputText {
                            text: content,
                            annotations: [],
                        }],
                    }));
                }
                for call in message.tool_calls() {
                    input.push(ResponsesItem::FunctionCall(ResponsesFunctionCall {
                        kind: "function_call",
                        call_id: call.id.as_str(),
                        name: &call.function.name,
                        arguments: &call.function.arguments,
                    }));
                }
            }
            ModelMessage::ToolResults { .. } => input.push(ResponsesItem::FunctionCallOutput(
                ResponsesFunctionCallOutput {
                    kind: "function_call_output",
                    call_id: message.tool_call_id().unwrap_or_default(),
                    output: message.content().unwrap_or_default(),
                },
            )),
        }
    }
    let tools = tools
        .iter()
        .map(|tool| ResponsesTool {
            kind: "function",
            name: &tool.name,
            description: &tool.description,
            parameters: &tool.parameters,
        })
        .collect::<Vec<_>>();

    Ok(ResponsesRequest {
        model,
        store: false,
        stream: true,
        instructions,
        input,
        tools,
        tool_choice: "auto",
        parallel_tool_calls: true,
        include: ["reasoning.encrypted_content"],
        text: ResponsesText { verbosity: "low" },
        prompt_cache_key: options.prompt_cache_key(),
    })
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
            cache_write_tokens: None,
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

#[cfg(test)]
pub(crate) async fn request_body_json(
    model: &str,
    messages: &[ModelMessage],
    tools: &[ModelTool],
    options: &CompletionOptions,
) -> Result<Value, InferenceError> {
    let body = build_request_body(model, messages, tools, options).await?;
    let encoded = compact_json_body(&body).expect("the request body serializes");
    Ok(serde_json::from_slice(&encoded).expect("a compact request body is JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatgpt::ChatGptEndpoints;
    use crate::{
        blocking::BlockingModel,
        inference::ModelClient,
        mock::{MockResponse, MockServer},
        model::ChatModel,
    };
    use tempfile::TempDir;
    use tokio::sync::watch;

    fn fixture(responses: Vec<MockResponse>) -> (TempDir, MockServer, CodexClient) {
        let directory = TempDir::new().unwrap();
        write_credential(&directory, "initial", u64::MAX - 1);
        let server = MockServer::start(responses);
        let client = CodexClient::with_endpoints(
            "gpt-test",
            Some(&directory.path().join("auth.json")),
            Duration::from_secs(2),
            ChatGptEndpoints::local(&server.base_url()),
        )
        .unwrap();
        (directory, server, client)
    }

    #[test]
    fn a_large_native_item_stream_completes_without_duplicate_replay() {
        use std::fmt::Write as _;
        let count = 50_000;
        let mut body = String::new();
        for text in ["first", "final"] {
            for id in 0..count {
                writeln!(body, "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"reasoning\",\"id\":\"{id}\",\"text\":\"{text}\"}}}}\n").unwrap();
            }
        }
        body.push_str("data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\ndata: {\"type\":\"response.completed\"}\n\n");
        let turn = replay_transcript(&body, &mut |_| ControlFlow::Continue(())).unwrap();
        let replay = turn.codex_items().unwrap();
        assert_eq!(replay.len(), count);
        for (id, item) in replay.iter().enumerate() {
            assert_eq!(item["id"], id.to_string());
            assert_eq!(item["text"], "final");
        }
    }

    #[test]
    fn native_item_updates_preserve_first_seen_order_and_do_not_repeat_text() {
        let mut reducer = CodexReducer::default();
        for item in [
            serde_json::json!({"type":"message","id":"z","content":[{"text":"first"}],"extra":true}),
            serde_json::json!({"type":"reasoning","id":"a","text":"initial"}),
            serde_json::json!({"type":"message","content":[{"text":"unindexed"}]}),
            serde_json::json!({"type":"reasoning","id":"a","text":"final"}),
            serde_json::json!({"type":"message","id":"z","content":[{"text":"replacement"}]}),
        ] {
            reducer.finish_item(item).unwrap();
        }
        let completed = serde_json::json!({"type":"response.completed","response":{"output":[
            {"type":"message","id":"z","content":[{"text":"replacement"}]},
            {"type":"message","content":[{"text":"unindexed"}]},
            {"type":"message","content":[{"text":"again"}]},
        ]}});
        assert!(
            reducer
                .apply(&completed.to_string(), &mut |_| ControlFlow::Continue(()))
                .unwrap()
                .is_break()
        );
        let turn = reducer.finish(&ClientIdentity::new()).unwrap();
        assert_eq!(turn.content.as_deref(), Some("firstunindexedagain"));
        let replay = turn.codex_items().unwrap();
        assert_eq!(replay.len(), 4);
        assert_eq!(replay[0]["id"], "z");
        assert_eq!(replay[0]["extra"], true);
        assert_eq!(replay[0]["content"][0]["text"], "replacement");
        assert_eq!(
            replay[1],
            serde_json::json!({"type":"reasoning","id":"a","text":"final"})
        );
        assert_eq!(replay[2]["content"][0]["text"], "unindexed");
        assert_eq!(replay[3]["content"][0]["text"], "again");
    }

    #[tokio::test]
    async fn codex_trace_records_one_exchange_and_never_invents_usage_or_tool_only_ttft() {
        use tracing::instrument::WithSubscriber as _;
        for transcript in [
            include_str!("fixtures/codex-tool.sse"),
            include_str!("fixtures/codex-answer.sse"),
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        ] {
            let (_directory, _server, client) = fixture(vec![MockResponse::sse(transcript)]);
            let client = client.with_name("codex-trace");
            let capture = crate::trace_capture::TraceCapture::default();
            let turn = generate(&client, &control())
                .with_subscriber(capture.subscriber())
                .await
                .unwrap();
            capture.assert_exchange("chatgpt-subscription", "codex-responses");
            assert_eq!(
                capture.field("model.name").as_deref(),
                Some("\"codex-trace\"")
            );
            assert_eq!(capture.field("model.stream").as_deref(), Some("true"));
            assert_eq!(
                capture.field("cache.style").as_deref(),
                Some("\"automatic\"")
            );
            assert!(capture.field("timing.first_event_ms").is_some());
            assert_eq!(
                capture.field("stream.first_delta_ms").is_some(),
                turn.content.is_some()
            );
            assert_eq!(
                capture.field("usage.input_tokens"),
                turn.usage
                    .and_then(|u| u.input_tokens)
                    .map(|n| n.to_string())
            );
            assert_eq!(
                capture.field("usage.cached_input_tokens"),
                turn.usage
                    .and_then(|u| u.cached_input_tokens)
                    .map(|n| n.to_string())
            );
            assert_eq!(
                capture.field("usage.reasoning_output_tokens"),
                turn.usage
                    .and_then(|u| u.reasoning_output_tokens)
                    .map(|n| n.to_string())
            );
            assert!(capture.field("usage.cache_write_tokens").is_none());
            if turn.usage.is_none() {
                assert!(!capture.text().contains("usage."));
            }
            assert_eq!(capture.field("output.partial").as_deref(), Some("false"));
            for secret in [
                "synthetic-access-initial",
                "synthetic-refresh-initial",
                "opaque",
            ] {
                assert!(!capture.text().contains(secret));
            }
        }
    }

    #[tokio::test]
    async fn a_public_codex_override_rejects_remote_hosts_and_invalidates_native_replay() {
        let (_directory, server, client) = fixture(vec![MockResponse::sse(include_str!(
            "fixtures/codex-tool.sse"
        ))]);
        let first = generate(&client, &control()).await.unwrap();
        let client = client.with_loopback_endpoint(&server.base_url()).unwrap();
        let error = client
            .generate(
                GenerateRequest {
                    messages: &[crate::model::assistant_message(&first)],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |_| ControlFlow::Continue(()),
                &control(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            InferenceError::Protocol(ProtocolFailure::ContinuationMismatch)
        ));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert!(matches!(
            client.with_loopback_endpoint("http://localhost:8000"),
            Err(InferenceError::InvalidRequest(
                RequestError::InvalidLoopbackEndpoint
            ))
        ));
    }

    #[tokio::test]
    async fn a_loopback_401_is_an_authentication_error_without_refresh() {
        let (_directory, server, client) = fixture(vec![MockResponse::failure(
            401,
            serde_json::json!({"error":"expired"}),
        )]);
        let client = client.with_loopback_endpoint(&server.base_url()).unwrap();
        assert!(matches!(
            generate(&client, &control()).await,
            Err(InferenceError::Authentication(AuthError::Provider(_)))
        ));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    fn write_credential(directory: &TempDir, suffix: &str, expires_at: u64) {
        std::fs::write(directory.path().join("auth.json"), serde_json::to_vec(&serde_json::json!({
            "version": 1, "access": format!("synthetic-access-{suffix}"),
            "refresh": format!("synthetic-refresh-{suffix}"), "expiresAt": expires_at, "accountId": "synthetic-account"
        })).unwrap()).unwrap();
    }

    async fn generate(
        client: &CodexClient,
        control: &TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        client
            .generate(
                GenerateRequest {
                    messages: &[ModelMessage::user("test")],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |_| ControlFlow::Continue(()),
                control,
            )
            .await
    }

    fn control() -> TurnControl {
        TurnControl::new(watch::channel(false).1, Duration::from_secs(2)).unwrap()
    }

    fn assert_redacted(
        error: &InferenceError,
        trace: &crate::trace_capture::TraceCapture,
        sentinels: &[&str],
    ) {
        let shown = format!("{error}\n{error:?}\n{}", trace.text());
        assert!(shown.contains("[REDACTED]"));
        assert!(trace.text().contains("model.complete"));
        for sentinel in sentinels {
            assert!(
                !shown.contains(sentinel),
                "a diagnostic retained a credential"
            );
        }
    }

    #[tokio::test]
    async fn unauthorized_diagnostics_and_metadata_exclude_initial_and_rotated_credentials() {
        use tracing::instrument::WithSubscriber as _;
        for status in [401, 403] {
            let echoed = "synthetic-access-initial synthetic-access-rotated";
            let refusal = MockResponse::failure(
                status,
                serde_json::json!({"error":{"message":echoed,"code":echoed}}),
            )
            .header("x-request-id", echoed);
            let responses = if status == 401 {
                vec![
                    MockResponse::failure(401, serde_json::json!({"error":echoed})),
                    refusal,
                ]
            } else {
                vec![refusal]
            };
            let (directory, server, client) = fixture(responses);
            if status == 401 {
                write_credential(&directory, "rotated", u64::MAX);
            }
            let trace = crate::trace_capture::TraceCapture::default();
            let error = generate(&client, &control())
                .with_subscriber(trace.subscriber())
                .await
                .unwrap_err();
            let InferenceError::Authentication(AuthError::Provider(context)) = &error else {
                panic!("expected authentication refusal")
            };
            assert_eq!(context.status, Some(status));
            assert!(context.diagnostic.contains("[REDACTED]"));
            assert!(context.code.as_ref().unwrap().contains("[REDACTED]"));
            assert!(context.request_id.as_ref().unwrap().contains("[REDACTED]"));
            let sentinels = if status == 401 {
                vec!["synthetic-access-initial", "synthetic-access-rotated"]
            } else {
                vec!["synthetic-access-initial"]
            };
            assert_redacted(&error, &trace, &sentinels);
            assert!(trace.text().contains("provider.request_id"));
            assert_eq!(
                server.requests.lock().unwrap().len(),
                if status == 401 { 2 } else { 1 }
            );
        }
    }

    #[tokio::test]
    async fn stream_error_metadata_excludes_both_credentials_after_a_resend() {
        use tracing::instrument::WithSubscriber as _;
        let echoed = "synthetic-access-initial synthetic-access-rotated";
        let body = format!(
            "data: {}\n\n",
            serde_json::json!({"error":{"code":echoed,"message":echoed},"response":{"model":echoed}})
        );
        let (directory, server, client) = fixture(vec![
            MockResponse::failure(401, serde_json::json!({"error":"expired"})),
            MockResponse::sse(&body).header("x-request-id", echoed),
        ]);
        write_credential(&directory, "rotated", u64::MAX);
        let trace = crate::trace_capture::TraceCapture::default();
        let error = generate(&client, &control())
            .with_subscriber(trace.subscriber())
            .await
            .unwrap_err();
        assert!(matches!(&error, InferenceError::Provider(context) if context.status == Some(200)));
        assert_redacted(
            &error,
            &trace,
            &["synthetic-access-initial", "synthetic-access-rotated"],
        );
        assert!(trace.text().contains("model.returned"));
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn serde_error_sources_cannot_quote_a_request_credential() {
        use tracing::instrument::WithSubscriber as _;
        let (_directory, _server, client) = fixture(vec![MockResponse::sse(
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"synthetic-access-initial\"}}\n\n",
        )]);
        let trace = crate::trace_capture::TraceCapture::default();
        let error = generate(&client, &control())
            .with_subscriber(trace.subscriber())
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            InferenceError::Protocol(ProtocolFailure::Decode(_))
        ));
        assert_redacted(&error, &trace, &["synthetic-access-initial"]);
    }

    #[tokio::test]
    async fn refresh_refusals_exclude_access_and_refresh_credentials_from_retained_sources() {
        use tracing::instrument::WithSubscriber as _;
        let echoed = "synthetic-access-initial synthetic-refresh-initial";
        let (_directory, server, client) = fixture(vec![
            MockResponse::failure(401, serde_json::json!({"error":"expired"})),
            MockResponse::failure(
                403,
                serde_json::json!({"error":echoed,"error_description":echoed}),
            ),
        ]);
        let trace = crate::trace_capture::TraceCapture::default();
        let error = generate(&client, &control())
            .with_subscriber(trace.subscriber())
            .await
            .unwrap_err();
        assert!(matches!(
            &error,
            InferenceError::Authentication(AuthError::Credential(ChatGptError::TokenRefused {
                status: 403,
                ..
            }))
        ));
        assert_redacted(
            &error,
            &trace,
            &["synthetic-access-initial", "synthetic-refresh-initial"],
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_zero_client_timeout_is_a_request_error_before_credential_io() {
        let directory = TempDir::new().unwrap();
        assert!(matches!(
            CodexClient::new(
                "fixture",
                Some(&directory.path().join("absent.json")),
                Duration::ZERO
            ),
            Err(InferenceError::InvalidRequest(RequestError::ZeroTimeout))
        ));
    }

    #[test]
    fn an_empty_client_model_is_a_request_error_before_credential_io() {
        let directory = TempDir::new().unwrap();
        assert!(matches!(
            CodexClient::new(
                " ",
                Some(&directory.path().join("absent.json")),
                Duration::from_secs(1)
            ),
            Err(InferenceError::InvalidRequest(RequestError::EmptyModel))
        ));
    }

    #[tokio::test]
    async fn chunks_can_split_utf8_lines_and_json_without_changing_the_turn() {
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"🍊\"}\r\n\r\ndata: {\"type\":\"response.completed\"}\r\n\r\n";
        let (_directory, server, client) = fixture(vec![MockResponse::sse(body).split(1)]);
        let turn = generate(&client, &control()).await.unwrap();
        assert_eq!(turn.content.as_deref(), Some("🍊"));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_body_with_no_events_and_returns_no_turn() {
        let (response, ready, release) = MockResponse::sse("body has not arrived").stalled();
        let (_directory, server, client) = fixture(vec![response]);
        let (cancel, signal) = watch::channel(false);
        let task = tokio::spawn(async move {
            generate(
                &client,
                &TurnControl::new(signal, Duration::from_secs(10)).unwrap(),
            )
            .await
        });
        ready.await.unwrap();
        cancel.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
        assert!(matches!(result, Err(InferenceError::Cancelled)));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn total_deadline_interrupts_a_stalled_body_without_a_retry() {
        let (response, ready, release) = MockResponse::sse("body has not arrived").stalled();
        let (_directory, server, client) = fixture(vec![response]);
        let task = tokio::spawn(async move {
            generate(
                &client,
                &TurnControl::new(watch::channel(false).1, Duration::from_millis(100)).unwrap(),
            )
            .await
        });
        ready.await.unwrap();
        let result = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .unwrap()
            .unwrap();
        release.send(()).unwrap();
        assert!(matches!(result, Err(InferenceError::DeadlineExceeded)));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_pre_cancelled_turn_sends_nothing() {
        let (_directory, server, client) = fixture(Vec::new());
        let result = generate(
            &client,
            &TurnControl::new(watch::channel(true).1, Duration::from_secs(1)).unwrap(),
        )
        .await;
        assert!(matches!(result, Err(InferenceError::Cancelled)));
        assert!(server.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn two_sessions_rejected_on_the_same_credential_rotate_once_and_both_resend_the_replacement()
     {
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        for stale_disk in [false, true] {
            let directory = TempDir::new().unwrap();
            write_credential(&directory, "initial", u64::MAX - 1);
            let token = format!("header.{}.signature", URL_SAFE_NO_PAD.encode(
            br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"synthetic-account"}}"#
        ));
            let oauth = MockServer::start(vec![MockResponse::json(serde_json::json!({
                "access_token": token, "refresh_token": "synthetic-refresh-replacement", "expires_in": 3600
            }))]);
            let (first_refusal, first_seen, release_first) =
                MockResponse::failure(401, serde_json::json!({})).held_before_headers();
            let (second_refusal, second_seen, release_second) =
                MockResponse::failure(401, serde_json::json!({})).held_before_headers();
            let server = MockServer::start(vec![
                first_refusal,
                second_refusal,
                MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
                MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
            ]);
            let mut endpoints = ChatGptEndpoints::local(&oauth.base_url());
            endpoints.responses = server.base_url();
            let client = Arc::new(ModelClient::Codex(
                CodexClient::with_endpoints(
                    "gpt-test",
                    Some(&directory.path().join("auth.json")),
                    Duration::from_secs(2),
                    endpoints,
                )
                .unwrap(),
            ));
            let (completed, mut completion) = tokio::sync::mpsc::unbounded_channel();
            let [first, second] = [(), ()].map(|()| {
                let bridge = BlockingModel::new(
                    Arc::clone(&client),
                    tokio::runtime::Handle::current(),
                    watch::channel(false).1,
                    Duration::from_secs(2),
                );
                let completed = completed.clone();
                tokio::task::spawn_blocking(move || {
                    let result = bridge.complete(
                        &[ModelMessage::user("test")],
                        &[],
                        &CompletionOptions::default(),
                        &mut |_| ControlFlow::Continue(()),
                    );
                    completed.send(()).unwrap();
                    result
                })
            });
            let (first, second, ()) = tokio::join!(first, second, async {
                first_seen.await.unwrap();
                second_seen.await.unwrap();
                release_first.send(()).unwrap();
                // The second 401 arrives only after the first turn installed and used B.
                completion.recv().await.unwrap();
                if stale_disk {
                    // An installed rotation is authoritative even if disk still holds A.
                    write_credential(&directory, "initial", u64::MAX - 1);
                }
                release_second.send(()).unwrap();
            });
            assert!(first.unwrap().unwrap().content.is_some());
            assert!(second.unwrap().unwrap().content.is_some());
            let requests = server.requests();
            assert_eq!(requests.len(), 4);
            for request in &requests[..2] {
                assert!(request.contains("Bearer synthetic-access-initial"));
            }
            for request in &requests[2..] {
                assert!(request.contains(&format!("Bearer {token}")));
            }
            assert_eq!(oauth.requests().len(), 1);
            assert!(oauth.requests()[0].contains("synthetic-refresh-initial"));
        }
    }

    #[tokio::test]
    async fn a_second_unauthorized_response_is_not_retried() {
        let (directory, server, client) = fixture(vec![
            MockResponse::failure(401, serde_json::json!({"error":"first"})),
            MockResponse::failure(401, serde_json::json!({"error":"second"})),
        ]);
        // Simulate another process rotating the credential after this client's snapshot.
        write_credential(&directory, "rotated", u64::MAX);
        let result = generate(&client, &control()).await;
        assert!(matches!(result, Err(InferenceError::Authentication(_))));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].split_once("\r\n\r\n").unwrap().1,
            requests[1].split_once("\r\n\r\n").unwrap().1
        );
        assert!(requests[0].contains("Bearer synthetic-access-initial"));
        assert!(requests[1].contains("Bearer synthetic-access-rotated"));
    }

    #[tokio::test]
    async fn rate_limits_retain_bounded_metadata_and_are_never_retried() {
        let (_directory, server, client) = fixture(vec![
            MockResponse::failure(
                429,
                serde_json::json!({"error":{"code":"quota","message":"wait"}}),
            )
            .header("retry-after", "7")
            .header("x-request-id", "req-1"),
        ]);
        let result = generate(&client, &control()).await;
        let Err(InferenceError::RateLimited(error)) = result else {
            panic!("expected rate limit")
        };
        assert_eq!(error.0.status, Some(429));
        assert_eq!(error.0.retry_after, Some(Duration::from_secs(7)));
        assert_eq!(error.0.request_id.as_deref(), Some("req-1"));
        assert_eq!(error.0.code.as_deref(), Some("quota"));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn provider_protocol_and_transport_failures_stay_distinct() {
        enum Expected {
            Provider,
            Protocol,
            Transport,
            Authentication,
        }
        for (response, expected) in [
            (
                MockResponse::failure(500, serde_json::json!({"error":"upstream failed"})),
                Expected::Provider,
            ),
            (
                MockResponse::failure(403, serde_json::json!({"error":"forbidden"})),
                Expected::Authentication,
            ),
            (MockResponse::hang_up(), Expected::Transport),
            (MockResponse::sse("data: not-json\n\n"), Expected::Protocol),
            (
                MockResponse::sse(
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                ),
                Expected::Protocol,
            ),
            (
                MockResponse::sse(
                    "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"content_filter\",\"message\":\"refused\"}}}\n\n",
                ),
                Expected::Provider,
            ),
            (
                MockResponse::sse("data: {\"type\":\"response.incomplete\"}\n\n"),
                Expected::Provider,
            ),
        ] {
            let (_directory, server, client) = fixture(vec![response]);
            let error = generate(&client, &control()).await.unwrap_err();
            assert!(matches!(
                (error, expected),
                (InferenceError::Provider(_), Expected::Provider)
                    | (InferenceError::Protocol(_), Expected::Protocol)
                    | (InferenceError::Transport(_), Expected::Transport)
                    | (InferenceError::Authentication(_), Expected::Authentication)
            ));
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn unknown_event_types_are_ignored_but_their_failures_are_not() {
        let body = concat!(
            "data: {\"type\":\"response.refusal.delta\",\"delta\":\"additive\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n",
            "data: {\"type\":\"unknown.additive.event\"}\n\n",
            "data: {\"type\":\"response.foo\",\"delta\":{\"x\":1}}\n\n",
            "data: {\"type\":\"x\",\"code\":429}\n\n",
            "data: {\"delta\":\"untyped\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"web_search_call\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"arguments\":{\"q\":1},\"name\":null}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"untyped\",\"content\":\"text\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"r\",\"content\":null}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"custom\",\"content\":\"text\"}]}}\n\n"
        );
        let (_directory, _server, client) = fixture(vec![MockResponse::sse(body)]);
        assert_eq!(
            generate(&client, &control())
                .await
                .unwrap()
                .content
                .as_deref(),
            Some("answer")
        );
        for event in [
            serde_json::json!({"type":"unknown.event","error":{"code":"quota","message":"refused"}}),
            serde_json::json!({"type":"unknown.event","response":{"error":{"code":"quota","message":"refused"}}}),
        ] {
            let mut reducer = CodexReducer::default();
            let result = reducer.apply(&event.to_string(), &mut |_| ControlFlow::Continue(()));
            let Err(InferenceError::Provider(failure)) = result else {
                panic!("expected provider failure");
            };
            assert_eq!(failure.code.as_deref(), Some("quota"));
        }
        for malformed in [
            r#"{"type":"response.output_text.delta","delta":{"x":1}}"#,
            r#"{"type":"response.output_item.done","item":{"type":"message","content":"text"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc","arguments":{}}}"#,
        ] {
            assert!(matches!(
                CodexReducer::default().apply(malformed, &mut |_| ControlFlow::Continue(())),
                Err(InferenceError::Protocol(_))
            ));
        }
    }

    #[tokio::test]
    async fn unfinished_call_arguments_never_become_an_executable_turn() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"tool\",\"arguments\":\"{\"}}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        );
        let (_directory, _server, client) = fixture(vec![MockResponse::sse(body)]);
        assert!(matches!(
            generate(&client, &control()).await,
            Err(InferenceError::Protocol(ProtocolFailure::Decode(_)))
        ));
    }

    #[tokio::test]
    async fn native_continuation_cannot_cross_configured_clients() {
        let (_directory, _server, client) = fixture(vec![MockResponse::sse(include_str!(
            "fixtures/codex-answer.sse"
        ))]);
        let turn = generate(&client, &control()).await.unwrap();
        let (_other_directory, other_server, other) = fixture(Vec::new());
        let result = other
            .generate(
                GenerateRequest {
                    messages: &[crate::model::assistant_message(&turn)],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |_| ControlFlow::Continue(()),
                &control(),
            )
            .await;
        assert!(matches!(
            result,
            Err(InferenceError::Protocol(
                ProtocolFailure::ContinuationMismatch
            ))
        ));
        assert!(other_server.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_reported_model_change_cannot_cross_a_bound_continuation() {
        let changed =
            include_str!("fixtures/codex-answer.sse").replace("gpt-test", "another-model");
        let (_directory, server, client) = fixture(vec![
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
            MockResponse::sse(&changed),
        ]);
        let turn = generate(&client, &control()).await.unwrap();
        let result = client
            .generate(
                GenerateRequest {
                    messages: &[crate::model::assistant_message(&turn)],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |_| ControlFlow::Continue(()),
                &control(),
            )
            .await;
        assert!(matches!(
            result,
            Err(InferenceError::Protocol(
                ProtocolFailure::ContinuationMismatch
            ))
        ));
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_error_without_an_event_type_is_still_a_provider_failure() {
        let (_directory, server, client) = fixture(vec![
            MockResponse::sse("data: {\"error\":{\"code\":\"refused\",\"message\":\"no\"}}\n\n")
                .header("x-request-id", "stream-request"),
        ]);
        let Err(InferenceError::Provider(error)) = generate(&client, &control()).await else {
            panic!("expected provider failure")
        };
        assert_eq!(error.status, Some(200));
        assert_eq!(error.code.as_deref(), Some("refused"));
        assert_eq!(error.request_id.as_deref(), Some("stream-request"));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn completed_output_reconciles_calls_and_preserves_ordered_text() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"tool\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"id\":\"one\",\"content\":[{\"text\":\"first\"}]},{\"type\":\"message\",\"id\":\"two\",\"content\":[{\"text\":\"second\"}]},{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"tool\",\"arguments\":\"{}\"}]}}\n\n"
        );
        let turn = replay_transcript(body, &mut |_| ControlFlow::Continue(())).unwrap();
        assert_eq!(turn.content.as_deref(), Some("firstsecond"));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.arguments, "{}");
        assert_eq!(turn.codex_items().unwrap().len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_bridges_share_a_client_but_never_a_cancellation_receiver() {
        let (response, ready, release) = MockResponse::sse("body has not arrived").stalled();
        let (_directory, _server, client) = fixture(vec![
            response,
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
        ]);
        let client = Arc::new(ModelClient::Codex(client));
        let (cancel, signal) = watch::channel(false);
        let first = BlockingModel::new(
            Arc::clone(&client),
            tokio::runtime::Handle::current(),
            signal,
            Duration::from_secs(2),
        );
        let second = BlockingModel::new(
            client,
            tokio::runtime::Handle::current(),
            watch::channel(false).1,
            Duration::from_secs(2),
        );
        let task = tokio::task::spawn_blocking(move || {
            first.complete(
                &[ModelMessage::user("first")],
                &[],
                &CompletionOptions::default(),
                &mut |_| ControlFlow::Continue(()),
            )
        });
        ready.await.unwrap();
        cancel.send(true).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_millis(500), task)
                .await
                .unwrap()
                .unwrap(),
            Err(InferenceError::Cancelled)
        ));
        release.send(()).unwrap();
        let answer = tokio::task::spawn_blocking(move || {
            second.complete(
                &[ModelMessage::user("second")],
                &[],
                &CompletionOptions::default(),
                &mut |_| ControlFlow::Continue(()),
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(answer.content.as_deref(), Some("Echoed hello."));
    }
}
