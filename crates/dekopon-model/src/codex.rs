use crate::{
    chatgpt::{ChatGptError, CredentialFile, ResolvedCredential, resolve_auth_path},
    control::TurnControl,
    diagnostic::DiagnosticSecrets,
    error::{
        AuthError, FailurePhase, InferenceError, ProtocolFailure, ProviderFailure, RequestError,
        TransportFailure,
    },
    http::{InferenceHttp, Progress, record_phase},
    inference::{GenerateRequest, InferenceModel, ReplayEvent},
    model::{
        AssistantTurn, CompletionOptions, ContentPart, DataUrl, JSON_CONTENT_TYPE,
        ModelFunctionCall, ModelKey, ModelMessage, ModelTool, ModelToolCall, sanitize_diagnostic,
    },
    sse::{SseEvent, decode_transcript},
    stream::{ModelText, TurnEvent},
};
use dekopon_model_token_governor::ModelUsage;
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

pub struct CodexClient {
    name: String,
    model: String,
    identity: parking_lot::RwLock<ModelKey>,
    credential: Arc<CredentialFile>,
    http: InferenceHttp,
    endpoint: String,
    loopback: bool,
}

impl CodexClient {
    pub fn new(
        model: impl Into<String>,
        auth_path: Option<&Path>,
        timeout: Duration,
        extra_ca_bundles: &[dekopon_core::Redacted<String>],
    ) -> Result<Self, InferenceError> {
        let model = model.into();
        Self::validate(&model, timeout)?;
        let path = resolve_auth_path(auth_path).map_err(AuthError::Credential)?;
        let credential = CredentialFile::open(&path, timeout).map_err(AuthError::Credential)?;
        Self::configured(
            model,
            Arc::new(credential),
            timeout,
            RESPONSES_URL.to_owned(),
            extra_ca_bundles,
        )
    }

    /// Clients on one credential file must share a single CredentialFile instance, or a sibling
    /// could spend a refresh token that an unwritten rotation already retired.
    pub fn with_credential(
        model: impl Into<String>,
        credential: Arc<CredentialFile>,
        timeout: Duration,
        extra_ca_bundles: &[dekopon_core::Redacted<String>],
    ) -> Result<Self, InferenceError> {
        let model = model.into();
        Self::validate(&model, timeout)?;
        Self::configured(
            model,
            credential,
            timeout,
            RESPONSES_URL.to_owned(),
            extra_ca_bundles,
        )
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
        credential: Arc<CredentialFile>,
        timeout: Duration,
        endpoint: String,
        extra_ca_bundles: &[dekopon_core::Redacted<String>],
    ) -> Result<Self, InferenceError> {
        Ok(Self {
            name: model.clone(),
            identity: parking_lot::RwLock::new(ModelKey::new(crate::model::Backend::Codex, &model)),
            model,
            credential,
            http: InferenceHttp::new(timeout, extra_ca_bundles)?,
            endpoint,
            loopback: false,
        })
    }

    pub fn model_key(&self) -> crate::model::ModelKey {
        self.identity.read().clone()
    }

    #[must_use]
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }

    pub fn with_loopback_endpoint(mut self, endpoint: &str) -> Result<Self, InferenceError> {
        self.endpoint = crate::loopback::endpoint(endpoint)?;
        self.loopback = true;
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn with_endpoints(
        model: impl Into<String>,
        auth_path: Option<&Path>,
        timeout: Duration,
        endpoints: crate::chatgpt::ChatGptEndpoints,
        extra_ca_bundles: &[dekopon_core::Redacted<String>],
    ) -> Result<Self, InferenceError> {
        let model = model.into();
        Self::validate(&model, timeout)?;
        let path = resolve_auth_path(auth_path).map_err(AuthError::Credential)?;
        let endpoint = endpoints.responses.clone();
        let credential = CredentialFile::with_endpoints(&path, timeout, endpoints)
            .map_err(AuthError::Credential)?;
        Self::configured(
            model,
            Arc::new(credential),
            timeout,
            endpoint,
            extra_ca_bundles,
        )
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
        observe: &mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
        control: &TurnControl,
        progress: &mut Progress,
        identity: &ModelKey,
    ) -> Result<AssistantTurn, InferenceError> {
        let span = tracing::Span::current();
        span.record("model.stream", true);
        span.record("cache.style", "automatic");
        let credentials = self.credential(RefreshMode::IfNeeded, control).await?;
        let body = control
            .run(build_request_body(
                &self.model,
                request.messages,
                request.tools,
                request.options,
                identity,
            ))
            .await
            .inspect_err(|_| record_phase(FailurePhase::BeforeSend))??;
        span.record("tool_choice", body.tool_choice);
        span.record("parallel_tool_calls", body.parallel_tool_calls);
        let continued_model = body
            .input
            .iter()
            .any(|item| match item {
                ResponsesItem::Replay(_) => true,
                ResponsesItem::Journal { message, model } => message.admits_opaque(model),
                ResponsesItem::Message(_)
                | ResponsesItem::FunctionCall(_)
                | ResponsesItem::FunctionCallOutput(_) => false,
            })
            .then(|| identity.clone());
        let encoded = body.chunks().map_err(TransportFailure::Encoding)?;
        drop(body);
        let build = || -> Result<_, InferenceError> {
            let body = crate::body::SliceBody::new(&encoded).map_err(TransportFailure::Body)?;
            Ok(builder_for_body(self, request.options).body(reqwest::Body::wrap(body)))
        };
        let mut secrets = DiagnosticSecrets::new(Some(&credentials.access));
        let refreshed;
        let mut response = self
            .http
            .send(
                build()?
                    .bearer_auth(credentials.access.expose())
                    .header("chatgpt-account-id", &credentials.account_id),
                control,
                secrets,
            )
            .await?;
        if response.status() == 401 && !self.loopback {
            drop(response);
            refreshed = self
                .credential(RefreshMode::Forced(credentials.access.clone()), control)
                .await?;
            secrets =
                DiagnosticSecrets::new(Some(&refreshed.access)).with_previous(&credentials.access);
            response = self
                .http
                .send(
                    build()?
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
            continued_model,
            ..CodexReducer::default()
        };
        response
            .sse(control, &mut |event| {
                progress.event();
                let SseEvent::Data(data) = event else {
                    return Ok(ControlFlow::Break(()));
                };
                reducer.apply_replay(data, &mut |event| match event {
                    ReplayEvent::ModelVerified => observe(ReplayEvent::ModelVerified),
                    ReplayEvent::Text(event) => {
                        progress.observe(event, &mut |event| observe(ReplayEvent::Text(event)))
                    }
                })
            })
            .await?;
        control
            .check()
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        let turn = reducer
            .finish(identity)
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        control
            .check()
            .inspect_err(|_| record_phase(FailurePhase::ReadingBody))?;
        tracing::Span::current().record("output.partial", false);
        if let Some(key) = turn.model_key() {
            *self.identity.write() = key.clone();
        }
        Ok(turn)
    }
}

impl crate::inference::ReplayModel for CodexClient {
    fn resolved_key(&self) -> ModelKey {
        self.model_key()
    }
    async fn exchange<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
        progress: &'a mut Progress,
        identity: &'a ModelKey,
    ) -> Result<AssistantTurn, InferenceError> {
        CodexClient::exchange(self, request, observe, control, progress, identity).await
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
                async {
                    let mut progress = Progress::new();
                    (
                        crate::inference::retry_opaque(
                            self,
                            request,
                            observe,
                            control,
                            &mut progress,
                        )
                        .await,
                        progress,
                    )
                },
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
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta,
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
    usage: Option<crate::wire::ResponsesUsage>,
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
}
enum WirePart<'a> {
    Reply(&'a str),
    Unknown(&'a str),
}

impl<'a> WirePart<'a> {
    fn parse(value: &'a Value) -> Result<Self, InferenceError> {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let field = match kind {
            "output_text" => "text",
            "refusal" => "refusal",
            kind => return Ok(Self::Unknown(kind)),
        };
        value
            .get(field)
            .and_then(Value::as_str)
            .map(Self::Reply)
            .ok_or_else(|| invalid_event("reply content part omitted text"))
    }
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
pub(crate) struct CodexReducer<'a> {
    secrets: DiagnosticSecrets<'a>,
    text: String,
    delta_messages: std::collections::HashSet<String>,
    unattributed_deltas: bool,
    native_items: Vec<Value>,
    native_positions: HashMap<String, usize>,
    calls: BTreeMap<String, PendingCall>,
    call_order: Vec<String>,
    terminal: Terminal,
    usage: Option<ModelUsage>,
    continued_model: Option<ModelKey>,
    reported_model: Option<String>,
}

impl CodexReducer<'_> {
    pub(crate) fn apply(
        &mut self,
        data: &str,
        observe: &mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
    ) -> Result<ControlFlow<()>, InferenceError> {
        self.apply_replay(data, &mut |event| match event {
            ReplayEvent::ModelVerified => ControlFlow::Continue(()),
            ReplayEvent::Text(event) => observe(event),
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "reshaped by the unit that next rewrites this"
    )]
    fn apply_replay(
        &mut self,
        data: &str,
        observe: &mut (dyn FnMut(ReplayEvent) -> ControlFlow<()> + Send),
    ) -> Result<ControlFlow<()>, InferenceError> {
        let event: Value =
            serde_json::from_str(data).map_err(|error| self.secrets.decode_failure(error))?;
        let kind = event
            .get("type")
            .and_then(|kind| EventKind::deserialize(kind).ok());
        let failed = match kind {
            Some(EventKind::Failed | EventKind::Incomplete | EventKind::Error) => true,
            Some(
                EventKind::ItemAdded
                | EventKind::ItemDone
                | EventKind::TextDelta
                | EventKind::RefusalDelta
                | EventKind::ArgumentsDelta
                | EventKind::ArgumentsDone
                | EventKind::Completed
                | EventKind::Created
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
                | EventKind::ReasoningTextDone,
            )
            | None => false,
        } || [event.get("error"), event.pointer("/response/error")]
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
            if self.continued_model.as_ref().is_some_and(|expected| {
                expected != &expected.clone().with_reported(Some(model.clone()))
            }) || self
                .reported_model
                .as_deref()
                .is_some_and(|prior| prior != model)
            {
                return Err(ProtocolFailure::ContinuationMismatch.into());
            }
            if self.reported_model.is_none() {
                tracing::Span::current().record("model.returned", model.as_str());
                self.reported_model = Some(model);
                if observe(ReplayEvent::ModelVerified).is_break() {
                    return Err(InferenceError::Cancelled);
                }
            }
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
                self.remember(&item)?;
            }
            EventKind::ItemDone => self.finish_item(
                event
                    .item
                    .ok_or_else(|| invalid_event("output item missing"))?,
            )?,
            EventKind::TextDelta | EventKind::RefusalDelta => {
                let delta = event
                    .delta
                    .ok_or_else(|| invalid_event("text delta missing"))?;
                if !delta.is_empty() {
                    if let Some(id) = event.item_id {
                        self.delta_messages.insert(id);
                    } else {
                        self.unattributed_deltas = true;
                    }
                    self.text.push_str(&delta);
                    if observe(ReplayEvent::Text(TurnEvent::TextDelta(
                        ModelText::from_model(delta),
                    )))
                    .is_break()
                    {
                        return Err(InferenceError::Cancelled);
                    }
                }
            }
            EventKind::ArgumentsDelta => {
                let call = self.argument_call(event.item_id.as_deref())?;
                call.arguments.push_str(
                    event
                        .delta
                        .as_deref()
                        .ok_or_else(|| invalid_event("argument delta missing"))?,
                );
            }
            EventKind::ArgumentsDone => {
                let call = self.argument_call(event.item_id.as_deref())?;
                if let Some(arguments) = event.arguments.filter(|value| !value.is_empty()) {
                    call.arguments = arguments;
                }
            }
            EventKind::Completed => {
                if let Some(response) = event.response {
                    if response.status.is_some_and(|status| match status {
                        ResponseStatus::Completed => false,
                        ResponseStatus::Queued
                        | ResponseStatus::InProgress
                        | ResponseStatus::Incomplete
                        | ResponseStatus::Failed
                        | ResponseStatus::Cancelled => true,
                    }) {
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

    fn argument_call(&mut self, item_id: Option<&str>) -> Result<&mut PendingCall, InferenceError> {
        let item_id = item_id
            .or_else(|| self.call_order.last().map(String::as_str))
            .ok_or_else(|| invalid_event("argument event without a call"))?;
        self.calls
            .get_mut(item_id)
            .ok_or_else(|| invalid_event("argument event for an unknown call"))
    }

    fn remember(&mut self, item: &WireItem<'_>) -> Result<(), InferenceError> {
        match item.kind {
            ItemKind::Message | ItemKind::Reasoning => return Ok(()),
            ItemKind::FunctionCall => {}
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
        match item.kind {
            ItemKind::Message => {
                let streamed = position.is_some()
                    || self.delta_messages.contains(item.id)
                    || self.unattributed_deltas;
                if position.is_none() {
                    self.unattributed_deltas = false;
                }
                let parts = match value.get("content") {
                    None | Some(Value::Null) => &[][..],
                    Some(content) => content
                        .as_array()
                        .ok_or_else(|| invalid_event("message content must be an array"))?
                        .as_slice(),
                };
                for part in parts {
                    match WirePart::parse(part)? {
                        WirePart::Reply(text) => {
                            if !streamed {
                                self.text.push_str(text);
                            }
                        }
                        WirePart::Unknown(kind) => {
                            tracing::debug!(event = "unknown_content_part_skipped", kind = %self.secrets.sanitize(kind), "unknown.content.part.skipped");
                        }
                    }
                }
            }
            ItemKind::FunctionCall => {
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
            ItemKind::Reasoning => {}
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

    pub(crate) fn finish(mut self, identity: &ModelKey) -> Result<AssistantTurn, InferenceError> {
        let reported = self.reported_model.clone().or_else(|| {
            self.continued_model
                .as_ref()
                .and_then(ModelKey::reported)
                .map(str::to_owned)
        });
        if self
            .continued_model
            .as_ref()
            .is_some_and(|expected| expected != &identity.clone().with_reported(reported.clone()))
        {
            return Err(ProtocolFailure::ContinuationMismatch.into());
        }
        match self.terminal {
            Terminal::Streaming => return Err(ProtocolFailure::MissingTerminal.into()),
            Terminal::Completed => {}
        }
        let mut calls = Vec::with_capacity(self.call_order.len());
        for id in self.call_order {
            if !self.native_positions.contains_key(&id) {
                return Err(invalid_event("unfinished function call"));
            }
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
        Ok(
            AssistantTurn::new(content, calls, self.usage).with_codex_continuation(
                self.native_items,
                identity.clone(),
                reported,
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
    reducer.finish(&ModelKey::new(crate::model::Backend::Codex, "transcript"))
}
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
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

fn builder_for_body(client: &CodexClient, options: &CompletionOptions) -> reqwest::RequestBuilder {
    let builder = client
        .http
        .post(&client.endpoint)
        .header("originator", "dekopon")
        .header(
            "user-agent",
            format!("dekopon/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("openai-beta", "responses=experimental")
        .header("accept", "text/event-stream")
        .header("content-type", JSON_CONTENT_TYPE);
    match options.prompt_cache_key() {
        Some(key) => builder.header("session-id", key),
        None => builder,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<&'a str>,
}

impl ResponsesRequest<'_> {
    fn chunks(&self) -> Result<Vec<crate::body::Chunk>, serde_json::Error> {
        let mut body = crate::body::JsonBody::default();
        body.raw(b"{\"model\":");
        body.value(self.model)?;
        body.raw(b",\"store\":false,\"stream\":true,\"instructions\":");
        body.value(&self.instructions)?;
        body.raw(b",\"input\":[");
        for (index, item) in self.input.iter().enumerate() {
            if index > 0 {
                body.raw(b",");
            }
            match item {
                ResponsesItem::Replay(value) => body.opaque(value)?,
                ResponsesItem::Journal { message, model } => message.responses(&mut body, model)?,
                ResponsesItem::Message(_)
                | ResponsesItem::FunctionCall(_)
                | ResponsesItem::FunctionCallOutput(_) => body.value(item)?,
            }
        }
        body.raw(b"],\"tools\":");
        body.value(&self.tools)?;
        body.raw(b",\"tool_choice\":");
        body.value(self.tool_choice)?;
        body.raw(b",\"parallel_tool_calls\":");
        body.value(&self.parallel_tool_calls)?;
        body.raw(b",\"include\":");
        body.value(&self.include)?;
        body.raw(b",\"text\":");
        body.value(&self.text)?;
        if let Some(key) = self.prompt_cache_key {
            body.raw(b",\"prompt_cache_key\":");
            body.value(key)?;
        }
        body.raw(b"}");
        Ok(body.finish())
    }
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

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ResponsesItem<'a> {
    Replay(&'a Value),
    #[serde(skip)]
    Journal {
        message: &'a crate::replay::ReplayMessage,
        model: ModelKey,
    },
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
        annotations: [Value; 0],
    },
}

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
                    Ok(bytes) => {
                        data.record_preparation(crate::asset::Preparation::Original);
                        Some(bytes)
                    }
                    Err(
                        crate::asset::BlobError::Reclaimed | crate::asset::BlobError::Unauthorized,
                    ) => {
                        data.record_preparation(crate::asset::Preparation::ReleaseNotice);
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
    identity: &ModelKey,
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
            ModelMessage::Journal { message } => {
                if !message.is_empty_for(identity) {
                    input.push(ResponsesItem::Journal {
                        message,
                        model: identity.clone(),
                    });
                }
            }
            ModelMessage::System { .. } => {}
            ModelMessage::User { .. } => input.push(ResponsesItem::Message(ResponsesMessage {
                kind: "message",
                role: "user",
                content: responses_content(message).await?,
            })),
            ModelMessage::Assistant { turn } => {
                if let Some(items) = turn.codex_items().filter(|items| {
                    !items.is_empty() && turn.model_key().is_some_and(|key| key == identity)
                }) {
                    input.extend(items.iter().map(ResponsesItem::Replay));
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

#[cfg(test)]
pub(crate) async fn request_body_json(
    model: &str,
    messages: &[ModelMessage],
    tools: &[ModelTool],
    options: &CompletionOptions,
) -> Result<Value, InferenceError> {
    let body = build_request_body(
        model,
        messages,
        tools,
        options,
        &ModelKey::new(crate::model::Backend::Codex, model),
    )
    .await?;
    let encoded = crate::body::collect(&body.chunks().expect("the request body encodes"))
        .await
        .expect("body slices read");
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
            &[],
        )
        .unwrap();
        (directory, server, client)
    }

    #[tokio::test]
    async fn preparation_tracks_each_occurrence_without_changing_raw_serialization() {
        use crate::asset::{BlobError, BlobReference, BlobSource, DiskBlob};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct ReadSequence {
            blob: DiskBlob,
            reads: AtomicUsize,
        }
        impl BlobSource for ReadSequence {
            fn pin(&self) -> Result<DiskBlob, BlobError> {
                if self.reads.fetch_add(1, Ordering::Relaxed) == 1 {
                    Err(BlobError::Unauthorized)
                } else {
                    Ok(self.blob.clone())
                }
            }
        }
        let source = Arc::new(ReadSequence {
            blob: DiskBlob::from_bytes(b"abc").unwrap(),
            reads: AtomicUsize::new(0),
        });
        let reference = BlobReference::new(Arc::clone(&source) as Arc<dyn BlobSource>, 3, 7);
        let message = ModelMessage::user_with_parts(vec![
            ContentPart::Image {
                mime: "image/png".into(),
                data: reference.clone(),
            },
            ContentPart::File {
                name: "photo.png".into(),
                mime: "image/png".into(),
                data: reference,
            },
        ]);
        let wire = serde_json::to_value(responses_content(&message).await.unwrap()).unwrap();
        assert_eq!(wire[0]["image_url"], "data:image/png;base64,YWJj");
        let raw = serde_json::to_value(&message).unwrap();
        let prepared = serde_json::to_value(message.prepared_for_trace()).unwrap();
        assert_eq!(
            prepared["content"][0],
            "[image/png, 3 bytes] sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(prepared["content"][1], wire[1]["text"]);
        assert_eq!(raw["content"][1], "[photo.png (image/png), 3 bytes]");
        assert_eq!(source.reads.load(Ordering::Relaxed), 2);

        let wire = serde_json::to_value(responses_content(&message).await.unwrap()).unwrap();
        assert_eq!(wire[1]["file_data"], "data:image/png;base64,YWJj");
        assert_eq!(
            serde_json::to_value(message.prepared_for_trace()).unwrap(),
            raw
        );
        assert_eq!(serde_json::to_value(&message).unwrap(), raw);
        assert_eq!(source.reads.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn message_snapshots_only_supply_text_for_messages_without_deltas() {
        for (delta, expected) in [
            (None, "firstsecond"),
            (Some(("m1", "first")), "firstsecond"),
            (Some(("m2", "second")), "secondfirst"),
        ] {
            let mut reducer = CodexReducer::default();
            if let Some((id, text)) = delta {
                let _flow = reducer
                    .apply(
                        &serde_json::json!({"type":"response.output_text.delta","item_id":id,"delta":text}).to_string(),
                        &mut |_| ControlFlow::Continue(()),
                    )
                    .unwrap();
            }
            for (id, text) in [("m1", "first"), ("m2", "second")] {
                reducer
                    .finish_item(serde_json::json!({"type":"message","id":id,
                    "content":[{"type":"output_text","text":text}]}))
                    .unwrap();
            }
            let _flow = reducer
                .apply(r#"{"type":"response.completed"}"#, &mut |_| {
                    ControlFlow::Continue(())
                })
                .unwrap();
            assert_eq!(
                reducer
                    .finish(&ModelKey::fixture())
                    .unwrap()
                    .content
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn an_added_call_must_complete_before_it_can_run() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"tool\",\"arguments\":\"{}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n"
        );
        assert!(matches!(
            replay_transcript(body, &mut |_| ControlFlow::Continue(())),
            Err(InferenceError::Protocol(_))
        ));
    }

    #[test]
    fn known_text_parts_require_text_and_unknown_parts_are_skipped() {
        let mut reducer = CodexReducer::default();
        assert!(matches!(
            reducer.finish_item(serde_json::json!({"type":"message",
            "content":[{"type":"output_text","annotations":[]}]})),
            Err(InferenceError::Protocol(_))
        ));
        reducer.finish_item(serde_json::json!({"type":"message","id":"m",
            "content":[{"type":"refusal","refusal":"declined"},{"type":"future","text":17},{"text":"untyped"}]})).unwrap();
        let _flow = reducer
            .apply(r#"{"type":"response.completed"}"#, &mut |_| {
                ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(
            reducer
                .finish(&ModelKey::fixture())
                .unwrap()
                .content
                .as_deref(),
            Some("declined")
        );
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
            serde_json::json!({"type":"message","id":"z","content":[{"type":"output_text","text":"first"}],"extra":true}),
            serde_json::json!({"type":"reasoning","id":"a","text":"initial"}),
            serde_json::json!({"type":"message","content":[{"type":"output_text","text":"unindexed"}]}),
            serde_json::json!({"type":"reasoning","id":"a","text":"final"}),
            serde_json::json!({"type":"message","id":"z","content":[{"type":"output_text","text":"replacement"}]}),
        ] {
            reducer.finish_item(item).unwrap();
        }
        let completed = serde_json::json!({"type":"response.completed","response":{"output":[
            {"type":"message","id":"z","content":[{"type":"output_text","text":"replacement"}]},
            {"type":"message","content":[{"type":"output_text","text":"unindexed"}]},
            {"type":"message","content":[{"type":"output_text","text":"again"}]},
        ]}});
        assert!(
            reducer
                .apply(&completed.to_string(), &mut |_| ControlFlow::Continue(()))
                .unwrap()
                .is_break()
        );
        let turn = reducer.finish(&ModelKey::fixture()).unwrap();
        assert_eq!(turn.content.as_deref(), Some("firstunindexedagain"));
        let replay = turn.codex_items().unwrap();
        assert_eq!(
            replay,
            &[serde_json::json!({"type":"reasoning","id":"a","text":"final"})]
        );
    }

    #[tokio::test]
    async fn codex_span_tool_options_equal_the_sent_body() {
        let (_directory, server, client) = fixture(vec![MockResponse::sse(include_str!(
            "fixtures/codex-answer.sse"
        ))]);
        let (result, span) =
            crate::trace_capture::exported_model_span(generate(&client, &control())).await;
        result.unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        crate::trace_capture::assert_tool_options(&span, &body);
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
    async fn a_public_codex_override_rejects_remote_hosts_and_keeps_model_identity() {
        let (_directory, server, client) = fixture(vec![
            MockResponse::sse(include_str!("fixtures/codex-tool.sse")),
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
        ]);
        let first = generate(&client, &control()).await.unwrap();
        let client = client.with_loopback_endpoint(&server.base_url()).unwrap();
        let answer = client
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
            .unwrap();
        assert_eq!(answer.content.as_deref(), Some("Echoed hello."));
        assert_eq!(server.requests.lock().len(), 2);
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
        assert_eq!(server.requests.lock().len(), 1);
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
        generate_with_options(client, &CompletionOptions::default(), control).await
    }

    async fn generate_with_options(
        client: &CodexClient,
        options: &CompletionOptions,
        control: &TurnControl,
    ) -> Result<AssistantTurn, InferenceError> {
        client
            .generate(
                GenerateRequest {
                    messages: &[ModelMessage::user("test")],
                    tools: &[],
                    options,
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
                server.requests.lock().len(),
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
        assert_eq!(server.requests.lock().len(), 2);
    }

    #[tokio::test]
    async fn a_cache_key_survives_a_credential_refresh_resend() {
        let (directory, server, client) = fixture(vec![
            MockResponse::failure(401, serde_json::json!({"error":"expired"})),
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
        ]);
        write_credential(&directory, "rotated", u64::MAX);
        generate_with_options(
            &client,
            &CompletionOptions::default().with_prompt_cache_key("session-7"),
            &control(),
        )
        .await
        .unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert!(
                request
                    .lines()
                    .any(|line| line.trim_end_matches('\r') == "session-id: session-7")
            );
            let (_, body) = request.split_once("\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["prompt_cache_key"], "session-7");
        }
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
        assert_eq!(server.requests.lock().len(), 2);
    }

    #[test]
    fn a_zero_client_timeout_is_a_request_error_before_credential_io() {
        let directory = TempDir::new().unwrap();
        assert!(matches!(
            CodexClient::new(
                "fixture",
                Some(&directory.path().join("absent.json")),
                Duration::ZERO,
                &[]
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
                Duration::from_secs(1),
                &[]
            ),
            Err(InferenceError::InvalidRequest(RequestError::EmptyModel))
        ));
    }

    #[tokio::test]
    async fn an_empty_completed_response_is_an_empty_turn() {
        let body = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":20,\"output_tokens\":5}}}\n\n";
        let (_directory, _server, client) = fixture(vec![MockResponse::sse(body)]);
        let turn = generate(&client, &control()).await.unwrap();
        assert_eq!(turn.content, None);
        assert!(turn.tool_calls.is_empty());
        let usage = turn.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[tokio::test]
    async fn chunks_can_split_utf8_lines_and_json_without_changing_the_turn() {
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"🍊\"}\r\n\r\ndata: {\"type\":\"response.completed\"}\r\n\r\n";
        let (_directory, server, client) = fixture(vec![MockResponse::sse(body).split(1)]);
        let turn = generate(&client, &control()).await.unwrap();
        assert_eq!(turn.content.as_deref(), Some("🍊"));
        assert_eq!(server.requests.lock().len(), 1);
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
        assert_eq!(server.requests.lock().len(), 1);
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
        assert_eq!(server.requests.lock().len(), 1);
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
        assert!(server.requests.lock().is_empty());
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
                    &[],
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
                completion.recv().await.unwrap();
                if stale_disk {
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
        write_credential(&directory, "rotated", u64::MAX);
        let result = generate(&client, &control()).await;
        assert!(matches!(result, Err(InferenceError::Authentication(_))));
        let requests = server.requests.lock();
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
        assert_eq!(server.requests.lock().len(), 1);
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
            assert_eq!(server.requests.lock().len(), 1);
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
            Some("additiveanswer")
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
    async fn a_new_client_for_the_same_model_replays_reasoning() {
        let (_directory, _server, client) = fixture(vec![MockResponse::sse(include_str!(
            "fixtures/codex-tool.sse"
        ))]);
        let turn = generate(&client, &control()).await.unwrap();
        let (_other_directory, other_server, other) = fixture(vec![
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
        ]);
        generate(&other, &control()).await.unwrap();
        assert_eq!(other.model_key(), *turn.model_key().unwrap());
        other
            .generate(
                GenerateRequest {
                    messages: &[crate::model::assistant_message(&turn)],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |_| ControlFlow::Continue(()),
                &control(),
            )
            .await
            .unwrap();
        let requests = other_server.requests();
        let body: serde_json::Value =
            serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "reasoning")
                .count(),
            turn.codex_items().unwrap().len()
        );
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call")
        );
    }

    #[tokio::test]
    async fn replayed_reasoning_streams_a_delta_before_the_response_completes() {
        let created =
            serde_json::json!({"type":"response.created","response":{"model":"gpt-test"}});
        let delta = serde_json::json!({"type":"response.output_text.delta","delta":"live canary"});
        let prefix = format!("data: {created}\n\ndata: {delta}\n\n");
        let terminal = serde_json::json!({"type":"response.completed","response":{"status":"completed","model":"gpt-test"}});
        let body = format!("{prefix}data: {terminal}\n\n");
        let (response, waiting, release) = MockResponse::sse(&body).stalled_after(prefix.len());
        let (_directory, server, client) = fixture(vec![
            MockResponse::sse(include_str!("fixtures/codex-tool.sse")),
            response,
        ]);
        let first = generate(&client, &control()).await.unwrap();
        let (seen, delta) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut seen = Some(seen);
            client
                .generate(
                    GenerateRequest {
                        messages: &[
                            crate::model::assistant_message(&first),
                            ModelMessage::tool(first.tool_calls[0].id.clone(), "done"),
                        ],
                        tools: &[],
                        options: &CompletionOptions::default(),
                    },
                    &mut |event| {
                        if let Some(seen) = seen.take() {
                            let _closed = seen.send(event);
                        }
                        ControlFlow::Continue(())
                    },
                    &control(),
                )
                .await
        });
        waiting.await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), delta).await;
        release.send(()).unwrap();
        let answer = task.await.unwrap().unwrap();
        let TurnEvent::TextDelta(text) = event
            .expect("delta before releasing terminal event")
            .unwrap();
        assert_eq!(text.as_str(), "live canary");
        assert_eq!(answer.content.as_deref(), Some("live canary"));
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let body: Value =
            serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning")
        );
    }

    #[tokio::test]
    async fn a_missing_then_present_reported_identity_retries_without_releasing_discarded_text() {
        let initial =
            include_str!("fixtures/codex-tool.sse").replace(",\"model\":\"gpt-test\"", "");
        let created =
            serde_json::json!({"type":"response.created","response":{"model":"gpt-test"}});
        let delta = serde_json::json!({"type":"response.output_text.delta","delta":"discarded"});
        let completed = serde_json::json!({"type":"response.completed","response":{"status":"completed","model":"gpt-test"}});
        let changed = format!("data: {created}\n\ndata: {delta}\n\ndata: {completed}\n\n");
        let replacement =
            include_str!("fixtures/codex-answer.sse").replace("Echoed hello.", "replacement");
        let (_directory, server, client) = fixture(vec![
            MockResponse::sse(&initial),
            MockResponse::sse(&changed),
            MockResponse::sse(&replacement),
        ]);
        let first = generate(&client, &control()).await.unwrap();
        assert_eq!(first.model_key().unwrap().reported(), None);
        let mut seen = String::new();
        let answer = client
            .generate(
                GenerateRequest {
                    messages: &[
                        crate::model::assistant_message(&first),
                        ModelMessage::tool(first.tool_calls[0].id.clone(), "done"),
                    ],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |event| {
                    let TurnEvent::TextDelta(text) = event;
                    seen.push_str(text.as_str());
                    ControlFlow::Continue(())
                },
                &control(),
            )
            .await
            .unwrap();
        assert_eq!(seen, "replacement");
        assert_eq!(answer.content.as_deref(), Some(seen.as_str()));
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let bodies: Vec<Value> = requests
            .iter()
            .map(|request| serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap())
            .collect();
        assert!(
            bodies[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning")
        );
        assert!(
            bodies[2]["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "reasoning")
        );
    }

    #[tokio::test]
    async fn a_reported_model_drift_retries_once_without_opaque_reasoning() {
        let changed =
            include_str!("fixtures/codex-answer.sse").replace("gpt-test", "another-model");
        let (_directory, server, client) = fixture(vec![
            MockResponse::sse(include_str!("fixtures/codex-tool.sse")),
            MockResponse::sse(&changed),
            MockResponse::sse(&changed),
        ]);
        let turn = generate(&client, &control()).await.unwrap();
        let mut events = Vec::new();
        let answer = client
            .generate(
                GenerateRequest {
                    messages: &[crate::model::assistant_message(&turn)],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |event| {
                    events.push(event);
                    ControlFlow::Continue(())
                },
                &control(),
            )
            .await
            .unwrap();
        assert_eq!(
            answer.model_key().unwrap().reported(),
            Some("another-model")
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        let bodies: Vec<Value> = requests
            .iter()
            .map(|request| serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap())
            .collect();
        assert!(
            bodies[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "reasoning")
        );
        assert!(
            bodies[2]["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "reasoning")
        );
        assert_eq!(
            bodies[1]["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] != "reasoning")
                .collect::<Vec<_>>(),
            bodies[2]["input"]
                .as_array()
                .unwrap()
                .iter()
                .collect::<Vec<_>>()
        );
        let text: String = events
            .iter()
            .map(|event| match event {
                TurnEvent::TextDelta(text) => text.as_str(),
            })
            .collect();
        assert_eq!(text, answer.content.as_deref().unwrap());
    }

    #[tokio::test]
    async fn changing_configured_models_replays_only_portable_text_and_calls() {
        let (_directory, _server, client) = fixture(vec![MockResponse::sse(include_str!(
            "fixtures/codex-tool.sse"
        ))]);
        let first = generate(&client, &control()).await.unwrap();
        let changed = include_str!("fixtures/codex-answer.sse").replace("gpt-test", "reported-b");
        let (_directory_b, server_b, mut client_b) = fixture(vec![MockResponse::sse(&changed)]);
        client_b.model = "configured-b".to_owned();
        client_b.identity =
            parking_lot::RwLock::new(ModelKey::new(crate::model::Backend::Codex, "configured-b"));
        let answer = client_b
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
            .unwrap();
        assert_eq!(answer.model_key().unwrap().reported(), Some("reported-b"));
        let requests = server_b.requests();
        assert_eq!(requests.len(), 1);
        let body: Value =
            serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "reasoning")
        );
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call")
        );
    }

    #[tokio::test]
    async fn unequal_reported_models_under_one_alias_strip_opaque_reasoning() {
        let (_directory, _server, client_a) = fixture(vec![MockResponse::sse(include_str!(
            "fixtures/codex-tool.sse"
        ))]);
        let turn_a = generate(&client_a, &control()).await.unwrap();
        let changed = include_str!("fixtures/codex-answer.sse").replace("gpt-test", "reported-b");
        let (_directory_b, server_b, client_b) = fixture(vec![
            MockResponse::sse(&changed),
            MockResponse::sse(&changed),
        ]);
        generate(&client_b, &control()).await.unwrap();
        assert_ne!(client_b.model_key(), *turn_a.model_key().unwrap());
        client_b
            .generate(
                GenerateRequest {
                    messages: &[crate::model::assistant_message(&turn_a)],
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |_| ControlFlow::Continue(()),
                &control(),
            )
            .await
            .unwrap();
        let requests = server_b.requests();
        assert_eq!(requests.len(), 2);
        let body: Value =
            serde_json::from_str(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "reasoning")
        );
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "function_call")
        );
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
        assert_eq!(server.requests.lock().len(), 1);
    }

    #[test]
    fn completed_output_reconciles_calls_and_preserves_ordered_text() {
        let body = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"tool\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"id\":\"one\",\"content\":[{\"type\":\"output_text\",\"text\":\"first\"}]},{\"type\":\"message\",\"id\":\"two\",\"content\":[{\"type\":\"output_text\",\"text\":\"second\"}]},{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"call\",\"name\":\"tool\",\"arguments\":\"{}\"}]}}\n\n"
        );
        let turn = replay_transcript(body, &mut |_| ControlFlow::Continue(())).unwrap();
        assert_eq!(turn.content.as_deref(), Some("firstsecond"));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.arguments, "{}");
        assert!(turn.codex_items().unwrap().is_empty());
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

    const SOURCES: [&str; 4] = ["user\né", "answer\n\\é", "{\"path\":\"é\"}", "result\n\\é"];

    fn journal_file(path: &std::path::Path, sources: &[String]) -> Vec<crate::text::Text> {
        let mut bytes = Vec::new();
        let mut spans = Vec::new();
        for source in sources {
            let encoded = serde_json::to_vec(source).unwrap();
            spans.push((bytes.len() as u64 + 1, encoded.len() as u64 - 2));
            bytes.extend_from_slice(&encoded);
            bytes.push(b'\n');
        }
        std::fs::write(path, &bytes).unwrap();
        let file = std::sync::Arc::new(std::fs::File::open(path).unwrap());
        spans
            .into_iter()
            .map(|(offset, len)| crate::text::Text::File {
                file: std::sync::Arc::clone(&file),
                offset,
                len,
            })
            .collect()
    }

    fn journal_turn(texts: Vec<crate::text::Text>) -> Vec<ModelMessage> {
        use crate::replay::{ReplayCall, ReplayMessage};
        let [user, answer, arguments, output] = <[_; 4]>::try_from(texts).unwrap();
        vec![
            ModelMessage::Journal {
                message: ReplayMessage::User(vec![user]),
            },
            ModelMessage::Journal {
                message: ReplayMessage::Assistant {
                    content: Some(answer),
                    calls: vec![ReplayCall {
                        id: "call-1".into(),
                        name: "bash".into(),
                        arguments,
                    }],
                    reasoning: Vec::new(),
                },
            },
            ModelMessage::Journal {
                message: ReplayMessage::Tool {
                    id: "call-1".into(),
                    text: output,
                },
            },
            ModelMessage::user("current"),
        ]
    }

    fn sources() -> Vec<String> {
        let mut sources: Vec<String> = SOURCES.iter().map(|source| (*source).to_owned()).collect();
        sources[3].push_str(&"x\n".repeat(80 * 1024));
        sources
    }

    fn wire_body(request: &str) -> &str {
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse().unwrap())
            })
            .unwrap();
        assert_eq!(body.len(), length);
        body
    }

    async fn send_turn(
        client: &CodexClient,
        messages: &[ModelMessage],
    ) -> Result<AssistantTurn, InferenceError> {
        client
            .generate(
                GenerateRequest {
                    messages,
                    tools: &[],
                    options: &CompletionOptions::default(),
                },
                &mut |_| ControlFlow::Continue(()),
                &control(),
            )
            .await
    }

    #[tokio::test]
    async fn a_compaction_between_projection_and_send_keeps_the_body_exact() {
        let (directory, server, client) = fixture(vec![MockResponse::sse(include_str!(
            "fixtures/codex-answer.sse"
        ))]);
        let path = directory.path().join("journal.jsonl");
        let sources = sources();
        let messages = journal_turn(journal_file(&path, &sources));
        let replacement = directory.path().join("journal.jsonl.tmp");
        std::fs::write(&replacement, b"{\"b\":[]}\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        send_turn(&client, &messages).await.unwrap();

        let owned = journal_turn(sources.into_iter().map(crate::text::Text::Owned).collect());
        let expected = request_body_json("gpt-test", &owned, &[], &CompletionOptions::default())
            .await
            .unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let sent: serde_json::Value = serde_json::from_str(wire_body(&requests[0])).unwrap();
        assert_eq!(sent, expected);
    }

    #[tokio::test]
    async fn a_retry_rebuilds_the_chunked_body() {
        let (directory, server, client) = fixture(vec![
            MockResponse::failure(401, serde_json::json!({"error":"expired"})),
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
        ]);
        write_credential(&directory, "rotated", u64::MAX);
        let path = directory.path().join("journal.jsonl");
        let messages = journal_turn(journal_file(&path, &sources()));
        send_turn(&client, &messages).await.unwrap();
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(wire_body(&requests[0]), wire_body(&requests[1]));
        drop(server);

        let (refusal, refused, release) =
            MockResponse::failure(401, serde_json::json!({"error":"expired"}))
                .held_before_headers();
        let (directory, server, client) = fixture(vec![
            refusal,
            MockResponse::sse(include_str!("fixtures/codex-answer.sse")),
        ]);
        write_credential(&directory, "rotated", u64::MAX);
        let path = directory.path().join("journal.jsonl");
        let messages = journal_turn(journal_file(&path, &sources()));
        let (result, ()) = tokio::join!(send_turn(&client, &messages), async {
            refused.await.unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(0)
                .unwrap();
            release.send(()).unwrap();
        });
        let Err(InferenceError::Transport(crate::error::TransportFailure::Body(error))) = result
        else {
            panic!("typed body failure: {result:?}");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(server.requests().len(), 1);
    }
}
