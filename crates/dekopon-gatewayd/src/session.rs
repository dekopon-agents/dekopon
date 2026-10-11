//! A session holds no authority of its own; it opens an attested broker leg naming the sender, and
//! an empty grant ends it before any model token is spent, whatever the message text says.

use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use dekopon_agent::{
    BrokerLeg, BrokerLegError, BudgetLimit, CancelSource, IdSequence, ProgressEvent, ProgressSink,
    ShellRuntime,
    attachment::{AssetDeliveryDisposition, ChatAssetInputs, ReplyAttachments},
    meta::{AgentConfigView, MemoryConfigView, MemoryScopeView, SessionConfigView, SkillView},
    prompt::{
        CancellationProbe, ConversationTurn, History, PromptError, ReplyDisposition, SessionInputs,
        Steer, SteerSource, run_prompt_session,
    },
};
use dekopon_broker_protocol::{
    Attestation, ChatScopeClaim, ClientError, ConversationKind, DeliveredAnswer,
    DeliveredTurnRequest, DeliveryIdentity, ERROR_STORAGE_BUSY, ERROR_STORAGE_CORRUPT,
    ERROR_STORAGE_IO, ERROR_STORAGE_QUOTA, ERROR_STORAGE_TIMEOUT, ERROR_UNAUTHENTICATED,
    InvocationOutcome, InvocationResult,
};
use dekopon_core::{ExternalSubject, PrincipalId};
use dekopon_model::error::InferenceError;
use dekopon_model::{
    blocking::BlockingModel,
    codex::CodexClient,
    inference::ModelClient,
    model::{ChatModel, CompletionOptions},
    openai::OpenAiClient,
};
use dekopon_process::{CancelHandle, CancelSignal};
use dekopon_shell::{CallBudget, CapabilityInvoker as _, Limits as ShellLimits};
use futures_util::StreamExt as _;
use thiserror::Error;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tracing::Instrument as _;

use crate::{
    asset::{self, AssetAccess, AssetStore, RecalledAsset, SessionAssets},
    collection::Dispositions,
    config::{
        MemoryPolicy, MemoryScope, MemoryWindow, ModelConfig, RecallSource, ResolvedBroker,
        ResolvedLiveness, Steering,
    },
    conversation::{
        ConversationKey, ConversationSeed, ConversationStore, EvictionReason, Residency,
        SealedConversation, TakenIn, Watermark,
    },
    jobs::JobContext,
    journal::{self, Journal},
    metering::ChatProviderGovernor,
    progress::{ProgressInputs, ProgressPolicy, StopCause, Terminal},
    routes::BoundRoute,
    transport::{
        AssetFetcher, CancelRequest, ChatDriver, InboundMessage, MessageId, OutboundReply,
        PastMessage, ThreadOwnership, TransportError, bound_inbound, bound_outbound,
        credential_from,
    },
    wake::{Anchor, SessionWakes},
};

pub(crate) const UNAUTHORIZED_REPLY: &str = "You're not authorized to use this agent.";
pub(crate) const BUSY_REPLY: &str = "I'm busy — try again shortly.";
pub(crate) const FAILURE_REPLY: &str = "The agent could not complete this request.";
pub(crate) const UNREPORTED_WORK_REPLY: &str = "The agent attempted capability work but could not report the result. Check the audit before retrying.";
pub(crate) const STOPPED_REPLY: &str = "Stopped.";
pub(crate) const WALL_CLOCK_REPLY: &str =
    "Stopped: this session reached its time limit. Capability calls already made were not undone.";
pub(crate) const MODEL_DEADLINE_REPLY: &str =
    "Stopped: the model did not answer in time. Capability calls already made were not undone.";
pub(crate) const EMPTY_ANSWER_REPLY: &str =
    "Stopped: the model returned an empty answer. Capability calls already made were not undone.";
pub(crate) const MAX_STEPS_REPLY: &str =
    "Stopped: this session reached its step limit. Capability calls already made were not undone.";
pub(crate) const SESSION_TASK_REPLY: &str =
    "Stopped: the gateway lost this session's task. Capability calls already made were not undone.";
pub(crate) const SEALED_THREAD_REPLY: &str = "Sorry, this agent took too long and we've canceled the chat. Please feel free to start a new one with a smaller scope.";
pub(crate) const EMPTY_REPLY: &str = "[empty response]";

const PLATFORM_RECALL_MAX_MESSAGES: usize = 100;
const PRINCIPAL_LOOKUPS_IN_FLIGHT: usize = 4;
// A history read that outlasts this is abandoned; the message is answered from an empty window.
const PLATFORM_RECALL_TIMEOUT: Duration = Duration::from_secs(5);

const SESSION_RUNNING: u8 = 0;
const SESSION_CANCELLED: u8 = 1;
const SESSION_COMPLETING: u8 = 2;

type AdmissionKey = (String, String);

pub(crate) type SharedModel = Arc<dyn ChatModel + Send + Sync>;

pub(crate) trait ModelFactory: Send + Sync {
    fn build(
        &self,
        model: &ModelConfig,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError>;
}

#[derive(Default)]
pub(crate) struct ConfiguredModels {
    clients: Mutex<HashMap<String, Arc<ModelClient>>>,
    /// One credential instance per auth file path, shared across every model naming it, so a second
    /// instance cannot spend a refresh token the first already rotated and retired.
    chatgpt_credentials:
        Mutex<HashMap<std::path::PathBuf, Arc<dekopon_model::chatgpt::CredentialFile>>>,
}

pub(crate) fn model_bearer_token(
    model: &ModelConfig,
) -> Result<Option<String>, ModelCredentialError> {
    model_bearer_token_with(model, |variable| std::env::var_os(variable))
}

pub(crate) fn model_bearer_token_with(
    model: &ModelConfig,
    resolve: impl FnOnce(&str) -> Option<std::ffi::OsString>,
) -> Result<Option<String>, ModelCredentialError> {
    let variable = match model {
        ModelConfig::OpenaiCompatible { api_key_env, .. } => api_key_env.as_deref(),
        ModelConfig::Openrouter { api_key_env, .. }
        | ModelConfig::Anthropic { api_key_env, .. } => Some(api_key_env.as_str()),
        ModelConfig::ChatgptSubscription { .. } => None,
    };
    match variable {
        Some(variable) => model_credential(model.name(), variable, resolve(variable)).map(Some),
        None => Ok(None),
    }
}

pub(crate) fn model_credential(
    model: &str,
    variable: &str,
    value: Option<std::ffi::OsString>,
) -> Result<String, ModelCredentialError> {
    credential_from(variable, value).map_err(|source| ModelCredentialError {
        model: model.to_owned(),
        variable: variable.to_owned(),
        source,
    })
}

impl ConfiguredModels {
    pub(crate) fn chatgpt_credential(
        &self,
        auth_file: Option<&std::path::Path>,
        timeout: std::time::Duration,
    ) -> Result<Arc<dekopon_model::chatgpt::CredentialFile>, InferenceError> {
        use dekopon_model::{chatgpt, error::AuthError};
        let path = chatgpt::resolve_auth_path(auth_file).map_err(AuthError::Credential)?;
        let cached = self.chatgpt_credentials.lock().get(&path).cloned();
        if let Some(credential) = cached {
            return Ok(credential);
        }
        // Opened outside the lock; if two models race to open the same file, the first instance
        // wins and is the one handed to every later model.
        let opened =
            Arc::new(chatgpt::CredentialFile::open(&path, timeout).map_err(AuthError::Credential)?);
        let mut credentials = self.chatgpt_credentials.lock();
        Ok(Arc::clone(credentials.entry(path).or_insert(opened)))
    }

    fn construct(&self, model: &ModelConfig) -> Result<Arc<ModelClient>, SessionError> {
        self.construct_with(model, |variable| std::env::var_os(variable))
    }

    fn construct_with(
        &self,
        model: &ModelConfig,
        mut resolve: impl FnMut(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Arc<ModelClient>, SessionError> {
        match model {
            ModelConfig::OpenaiCompatible {
                name,
                endpoint,
                model,
                api_key_env,
                timeout_ms,
                stream,
                ..
            } => {
                let bearer_token = api_key_env
                    .as_deref()
                    .map(|variable| model_credential(model, variable, resolve(variable)))
                    .transpose()?;
                Ok(Arc::new(ModelClient::OpenAiCompatible(
                    OpenAiClient::new(
                        endpoint,
                        model,
                        bearer_token,
                        std::time::Duration::from_millis(*timeout_ms),
                    )?
                    .with_streaming(*stream)
                    .with_name(name),
                )))
            }
            ModelConfig::Openrouter {
                name,
                model,
                api_key_env,
                timeout_ms,
                generation,
                reasoning,
                routing,
                cache,
                ..
            } => {
                let token = model_credential(name, api_key_env, resolve(api_key_env))?;
                let settings = dekopon_model::openrouter::settings::Settings {
                    generation: generation.clone(),
                    reasoning: reasoning.clone(),
                    routing: routing.clone(),
                    cache: cache.clone(),
                };
                Ok(Arc::new(ModelClient::OpenRouter(
                    dekopon_model::openrouter::OpenRouterClient::new(
                        model,
                        token,
                        std::time::Duration::from_millis(*timeout_ms),
                        settings,
                    )?
                    .with_name(name),
                )))
            }
            ModelConfig::ChatgptSubscription {
                name,
                model,
                auth_file,
                timeout_ms,
                ..
            } => {
                let timeout = std::time::Duration::from_millis(*timeout_ms);
                let credential = self.chatgpt_credential(auth_file.as_deref(), timeout)?;
                Ok(Arc::new(ModelClient::Codex(
                    CodexClient::with_credential(model, credential, timeout)?.with_name(name),
                )))
            }
            ModelConfig::Anthropic { name, .. } => Err(SessionError::ProxyOnlyModel {
                model: name.clone(),
            }),
        }
    }
}

impl ModelFactory for ConfiguredModels {
    fn build(
        &self,
        model: &ModelConfig,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        let cached = self.clients.lock().get(model.name()).cloned();
        let client = match cached {
            Some(client) => client,
            None => {
                let built = self.construct(model)?;
                let mut clients = self.clients.lock();
                Arc::clone(clients.entry(model.name().to_owned()).or_insert(built))
            }
        };
        let timeout_ms = model
            .timeout_ms()
            .ok_or_else(|| SessionError::ProxyOnlyModel {
                model: model.name().to_owned(),
            })?;
        Ok(Arc::new(BlockingModel::new(
            client,
            runtime,
            cancel,
            std::time::Duration::from_millis(timeout_ms),
        )))
    }
}

pub(crate) struct ModelCache {
    factory: Arc<dyn ModelFactory>,
}
impl ModelCache {
    pub(crate) fn new(factory: Arc<dyn ModelFactory>) -> Self {
        Self { factory }
    }
    pub(crate) fn client(
        &self,
        model: &ModelConfig,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        self.factory.build(model, runtime, cancel)
    }
}

pub(crate) const MAILBOX_CAPACITY: usize = 8;

#[derive(Clone)]
pub(crate) struct SessionGate {
    permits: Arc<Semaphore>,
    refusals: Arc<Semaphore>,
    in_flight: Arc<Mutex<BTreeMap<AdmissionKey, Holder>>>,
}

enum Holder {
    Probe,
    Session(Box<Running>),
}

struct Running {
    subject: dekopon_core::ExternalSubject,
    cancellation: SessionCancellation,
    steering: Steering,
    aborts: u8,
    received_at: tokio::time::Instant,
    steers: VecDeque<InboundMessage>,
    follow_ups: VecDeque<FollowUp>,
}

impl Running {
    fn new(route: &BoundRoute, message: &InboundMessage) -> Self {
        Self {
            subject: message.subject.clone(),
            cancellation: SessionCancellation::new(),
            steering: route.steering,
            aborts: 0,
            received_at: message.received_at,
            steers: VecDeque::new(),
            follow_ups: VecDeque::new(),
        }
    }
}

pub(crate) struct FollowUp {
    pub route: BoundRoute,
    pub message: InboundMessage,
    pub receipts: Dispositions,
}

pub(crate) enum Admit {
    Admitted(SessionAdmission, InboundMessage, Dispositions),
    Steered(Dispositions),
    Queued,
    Full(InboundMessage, Dispositions),
    Saturated(InboundMessage, Dispositions),
}

pub(crate) struct StopOutcome {
    pub running: CancelOutcome,
    pub dropped: bool,
}

fn record_admission(message: &InboundMessage, outcome: &str, depth: usize, cause: Option<&str>) {
    if let Some(cause) = cause {
        tracing::Span::current().record("busy.cause", cause);
    }
    tracing::info!(
        target: "dekopon_gatewayd::audit",
        { audit.event = "gateway.admission", outcome, busy.cause = cause,
          transport = %message.transport, conversation.id = %message.conversation.key(),
          queue.depth = depth },
        "gateway admission"
    );
}

impl SessionGate {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent)),
            refusals: Arc::new(Semaphore::new(max_concurrent)),
            in_flight: Arc::default(),
        }
    }

    pub(crate) fn admit(
        &self,
        route: &BoundRoute,
        message: InboundMessage,
        receipts: Dispositions,
    ) -> Admit {
        let key = (message.transport.clone(), message.conversation.key());
        let mut entries = self.in_flight.lock();
        if let Some(holder) = entries.get_mut(&key) {
            let Holder::Session(running) = holder else {
                record_admission(&message, "busy", 0, Some("saturated"));
                return Admit::Saturated(message, receipts);
            };
            let depth = running.steers.len() + running.follow_ups.len();
            if depth == MAILBOX_CAPACITY {
                record_admission(&message, "busy", depth, Some("same-conversation"));
                tracing::info!(event = "gateway_steer_refused", reason = "mailbox-full");
                return Admit::Full(message, receipts);
            }
            if message.subject == running.subject
                && match message.message_id {
                    MessageId::Native(_) | MessageId::Job { .. } => true,
                    MessageId::Wake { .. } => false,
                }
                && running.cancellation.state.load(Ordering::Acquire) == SESSION_RUNNING
            {
                let interrupts = match message.message_id {
                    MessageId::Native(_) | MessageId::Wake { .. } => true,
                    MessageId::Job { .. } => false,
                };
                record_admission(&message, "steered", depth + 1, None);
                running.steers.push_back(message);
                if running.steering == Steering::Abort
                    && interrupts
                    && usize::from(running.aborts) < MAILBOX_CAPACITY
                {
                    running.aborts += 1;
                    running.cancellation.interrupt_model();
                }
                return Admit::Steered(receipts);
            }
            record_admission(&message, "queued", depth + 1, None);
            running.follow_ups.push_back(FollowUp {
                route: route.clone(),
                message,
                receipts,
            });
            return Admit::Queued;
        }
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            record_admission(&message, "busy", 0, Some("saturated"));
            return Admit::Saturated(message, receipts);
        };
        let running = Running::new(route, &message);
        let cancellation = running.cancellation.clone();
        entries.insert(key.clone(), Holder::Session(Box::new(running)));
        Admit::Admitted(
            SessionAdmission {
                _permit: permit,
                key,
                in_flight: Arc::clone(&self.in_flight),
                cancellation,
                route: Box::new(route.clone()),
            },
            message,
            receipts,
        )
    }

    pub(crate) fn admit_probe(&self, key: AdmissionKey) -> Option<ProbeAdmission> {
        let mut entries = self.in_flight.lock();
        if entries.contains_key(&key) {
            return None;
        }
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        entries.insert(key.clone(), Holder::Probe);
        Some(ProbeAdmission {
            _permit: permit,
            key,
            in_flight: Arc::clone(&self.in_flight),
        })
    }

    pub(crate) fn take_steers(&self, key: &AdmissionKey) -> Vec<InboundMessage> {
        let mut entries = self.in_flight.lock();
        let Some(Holder::Session(running)) = entries.get_mut(key) else {
            return Vec::new();
        };
        running.cancellation.rearm_model();
        running.steers.drain(..).collect()
    }

    pub(crate) fn cancel(&self, request: &CancelRequest) -> StopOutcome {
        let key = (request.transport.clone(), request.conversation_id.clone());
        let mut entries = self.in_flight.lock();
        let Some(Holder::Session(running)) = entries.get_mut(&key) else {
            return StopOutcome {
                running: CancelOutcome::NoSession,
                dropped: false,
            };
        };
        let before = running.steers.len() + running.follow_ups.len();
        running.steers.retain(|message| {
            let keep = message.subject.canonical() != request.subject;
            if !keep {
                for receipt in &message.constituents {
                    crate::collection::disposition(receipt, "stopped");
                }
            }
            keep
        });
        running.follow_ups.retain_mut(|followup| {
            let keep = followup.message.subject.canonical() != request.subject;
            if !keep {
                for receipt in followup.receipts.0.drain(..) {
                    crate::collection::disposition(&receipt, "stopped");
                }
            }
            keep
        });
        let dropped = before != running.steers.len() + running.follow_ups.len();
        let outcome = if running.subject.canonical() != request.subject {
            CancelOutcome::OtherSubject
        } else if running
            .cancellation
            .cancel(CancelSource::User { via: request.via })
        {
            CancelOutcome::Cancelled
        } else if running.cancellation.is_cancelled() {
            CancelOutcome::AlreadyCancelled
        } else {
            CancelOutcome::Completing
        };
        StopOutcome {
            running: outcome,
            dropped,
        }
    }
}

impl SessionGate {
    pub fn refusal(&self) -> Option<OwnedSemaphorePermit> {
        let permit = Arc::clone(&self.refusals).try_acquire_owned().ok();
        if permit.is_none() {
            tracing::info!(
                event = "gateway_refusal_reply_skipped",
                reason = "refusals-full"
            );
        }
        permit
    }
}

pub(crate) struct SessionAdmission {
    _permit: OwnedSemaphorePermit,
    key: AdmissionKey,
    in_flight: Arc<Mutex<BTreeMap<AdmissionKey, Holder>>>,
    cancellation: SessionCancellation,
    route: Box<BoundRoute>,
}

impl SessionAdmission {
    pub(crate) fn cancellation(&self) -> SessionCancellation {
        self.cancellation.clone()
    }

    pub(crate) fn next_or_release(mut self) -> Option<(Self, FollowUp)> {
        let next = {
            let mut entries = self.in_flight.lock();
            let Some(Holder::Session(running)) = entries.get_mut(&self.key) else {
                return None;
            };
            // Fold before changing subject: leftover steers must never drain into another sender's run.
            while let Some(mut message) = running.steers.pop_front() {
                let folds = match message.message_id {
                    MessageId::Native(_) | MessageId::Wake { .. } => true,
                    MessageId::Job { .. } => false,
                };
                if folds {
                    let mut grouped = Vec::new();
                    while running
                        .steers
                        .front()
                        .is_some_and(|steer| match steer.message_id {
                            MessageId::Native(_) => true,
                            MessageId::Wake { .. } | MessageId::Job { .. } => false,
                        })
                    {
                        if let Some(next) = running.steers.pop_front() {
                            grouped.push(next);
                        }
                    }
                    message.text = bound_inbound(&crate::collection::combined_text(
                        std::iter::once(&message).chain(grouped.iter()),
                    ));
                    for next in grouped {
                        if let MessageId::Native(id) = next.message_id {
                            message.folded.push(id);
                        }
                        message.folded.extend(next.folded);
                        message.assets.extend(next.assets);
                        message.constituents.extend(next.constituents);
                        message.receive_span = next.receive_span;
                    }
                }
                running.follow_ups.push_back(FollowUp {
                    route: (*self.route).clone(),
                    receipts: crate::collection::Dispositions(message.constituents.clone()),
                    message,
                });
            }
            if let Some(next) = running.follow_ups.pop_front() {
                running.subject = next.message.subject.clone();
                running.cancellation = SessionCancellation::new();
                running.steering = next.route.steering;
                running.aborts = 0;
                running.received_at = next.message.received_at;
                self.cancellation = running.cancellation.clone();
                *self.route = next.route.clone();
                Some(next)
            } else {
                entries.remove(&self.key);
                None
            }
        };
        next.map(|next| (self, next))
    }
}

impl Drop for SessionAdmission {
    fn drop(&mut self) {
        let mut entries = self.in_flight.lock();
        if matches!(entries.get(&self.key), Some(Holder::Session(running))
            if Arc::ptr_eq(&running.cancellation.state, &self.cancellation.state))
        {
            entries.remove(&self.key);
        }
    }
}

pub(crate) struct ProbeAdmission {
    _permit: OwnedSemaphorePermit,
    key: AdmissionKey,
    in_flight: Arc<Mutex<BTreeMap<AdmissionKey, Holder>>>,
}

impl Drop for ProbeAdmission {
    fn drop(&mut self) {
        self.in_flight.lock().remove(&self.key);
    }
}

#[derive(Clone)]
pub(crate) struct SessionCancellation {
    state: Arc<AtomicU8>,
    source: Arc<Mutex<Option<CancelSource>>>,
    woken: Arc<Notify>,
    handle: CancelHandle,
    signal: CancelSignal,
    model: tokio::sync::watch::Sender<bool>,
}

impl SessionCancellation {
    pub(crate) fn new() -> Self {
        let (handle, signal) = CancelSignal::pair();
        Self {
            state: Arc::new(AtomicU8::new(SESSION_RUNNING)),
            source: Arc::new(Mutex::new(None)),
            woken: Arc::new(Notify::new()),
            handle,
            signal,
            model: tokio::sync::watch::channel(false).0,
        }
    }

    pub(crate) fn model_watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.model.subscribe()
    }

    pub(crate) fn interrupt_model(&self) {
        self.model.send_replace(true);
    }

    pub(crate) fn rearm_model(&self) {
        // Serialize with session cancellation so draining can never clear a stop.
        let _source = self.source.lock();
        if self.state.load(Ordering::Acquire) != SESSION_CANCELLED {
            self.model.send_replace(false);
        }
    }

    /// Idempotent and claimed twice on the answering path, so a stop landing in the gap between the
    /// loop finishing and the session resuming cannot write a stopped line under an answer already
    /// being read.
    #[must_use]
    pub(crate) fn claim_completion(&self) -> bool {
        match self.state.compare_exchange(
            SESSION_RUNNING,
            SESSION_COMPLETING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(state) => state == SESSION_COMPLETING,
        }
    }

    /// The cancel origin is written under the same lock that decides the race, so a caller that
    /// observes the cancelled state and then reads the origin sees the winner's write, never the
    /// prior absence.
    pub(crate) fn cancel(&self, source: CancelSource) -> bool {
        let cancelled = {
            let mut recorded = self.source.lock();
            let won = self
                .state
                .compare_exchange(
                    SESSION_RUNNING,
                    SESSION_CANCELLED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
            if won {
                *recorded = Some(source);
            }
            won
        };
        if cancelled {
            self.handle.cancel();
            self.model.send_replace(true);
            self.woken.notify_waiters();
        }
        cancelled
    }

    pub(crate) fn source(&self) -> Option<CancelSource> {
        *self.source.lock()
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            // Register the notified future before checking cancellation state, or a cancel arriving
            // between the two checks is silently missed.
            let woken = self.woken.notified();
            if self.state.load(Ordering::Acquire) == SESSION_CANCELLED {
                return;
            }
            woken.await;
        }
    }

    pub(crate) fn signal(&self) -> CancelSignal {
        self.signal.clone()
    }
}

impl CancellationProbe for SessionCancellation {
    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == SESSION_CANCELLED
    }

    fn cancel_source(&self) -> Option<CancelSource> {
        self.source()
    }
}

struct CancellationOnDrop(SessionCancellation);

impl Drop for CancellationOnDrop {
    fn drop(&mut self) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "the bool says whether this drop won the race; a session that already \
                      completed or was already stopped needs nothing more done about it"
        )]
        let _ = self.0.cancel(CancelSource::Operator);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancelOutcome {
    Cancelled,
    NoSession,
    OtherSubject,
    AlreadyCancelled,
    Completing,
}

impl CancelOutcome {
    pub(crate) const fn ignored_reason(self) -> Option<&'static str> {
        match self {
            Self::Cancelled => None,
            Self::NoSession => Some("no-session"),
            Self::OtherSubject => Some("other-subject"),
            Self::AlreadyCancelled | Self::Completing => Some("already-ended"),
        }
    }
}

pub(crate) struct PendingNotice(StopCause);

pub(crate) struct SessionRunner {
    pub broker: ResolvedBroker,
    pub models: Arc<ModelCache>,
    pub gate: SessionGate,
    pub reply_on_busy: bool,
    pub conversations: ConversationStore,
    pub pending_notices: Mutex<HashMap<ConversationKey, PendingNotice>>,
    pub sealed_conversations: Mutex<HashMap<ConversationKey, SealedConversation>>,
    pub journal: Option<Arc<Journal>>,
    pub assets: Arc<AssetStore>,
    pub asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>>,
    pub liveness: BTreeMap<String, Arc<ResolvedLiveness>>,
    pub thread_ownership: HashMap<String, Arc<dyn ThreadOwnership>>,
    pub wakes: Option<Arc<crate::wake::WakeStore>>,
    pub jobs: Arc<crate::jobs::Jobs>,
    pub metering: Arc<dekopon_model_token_governor::Metering>,
}

struct RecalledWindow {
    history: History,
    assets: Vec<RecalledAsset>,
    next_asset_id: u64,
    newest: Option<String>,
}

struct Delta {
    messages: Vec<PastMessage>,
    names: Names,
}

struct Names {
    principals: HashMap<ExternalSubject, PrincipalId>,
    scope: MemoryScope,
}

impl Names {
    // Authors come from the chat service, not the broker, so the label does not claim authentication.
    fn label(&self, message: &PastMessage) -> String {
        match (self.principals.get(&message.subject), self.scope) {
            (Some(principal), _) => format!("[gateway: chat history, from {principal}]"),
            (None, MemoryScope::SharedConversation) => {
                "[gateway: chat history, from unmapped participant]".to_owned()
            }
            (None, MemoryScope::PrivateConversation) => {
                format!("[gateway: chat history, from {}]", message.author)
            }
        }
    }
}

struct Recall<'a> {
    runner: &'a SessionRunner,
    route: &'a BoundRoute,
    message: &'a InboundMessage,
    driver: &'a dyn ChatDriver,
    sender: Option<&'a PrincipalId>,
    sealed_at: Option<SystemTime>,
}

impl Recall<'_> {
    async fn window(
        &self,
        key: &ConversationKey,
        granted: &[String],
        window: MemoryWindow,
    ) -> Option<RecalledWindow> {
        let span = tracing::Span::current();
        match window.recall {
            RecallSource::None => None,
            RecallSource::Journal => {
                let journal = Arc::clone(self.runner.journal.as_ref()?);
                let stem = key.journal_stem();
                let grant = journal::grant_digest(granted);
                let recalled = tokio::task::spawn_blocking(move || {
                    journal.recall(&stem, &grant, window, SystemTime::now())
                })
                .await;
                match recalled {
                    Ok(Ok(recalled)) => {
                        span.record("conversation.recall_source", "journal");
                        tracing::info!(
                            event = "gateway_recalled",
                            source = "journal",
                            messages = recalled.history.len(),
                            delta = 0
                        );
                        Some(RecalledWindow {
                            history: recalled.history,
                            assets: recalled.assets,
                            next_asset_id: recalled.next_asset_id,
                            newest: None,
                        })
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            event = "gateway_recall_failed",
                            source = "journal",
                            reason = error.label()
                        );
                        None
                    }
                    Err(_) => {
                        tracing::warn!(
                            event = "gateway_recall_failed",
                            source = "journal",
                            reason = "task"
                        );
                        None
                    }
                }
            }
            RecallSource::Platform => {
                let horizon = self
                    .sealed_at
                    .map_or(horizon(window), |at| horizon(window).max(at));
                // Without Discord's Message Content intent other people's messages arrive with no content.
                let (messages, names) = self
                    .platform(window, None, |message| {
                        message.at >= horizon
                            && (!message.text.trim().is_empty() || !message.assets.is_empty())
                    })
                    .await?;
                span.record("conversation.recall_source", "platform");
                tracing::info!(
                    event = "gateway_recalled",
                    source = "platform",
                    messages = messages.len(),
                    delta = 0
                );
                Some(platform_window(messages, &names, window))
            }
        }
    }

    async fn delta(&self, window: MemoryWindow, watermark: &Watermark) -> Option<Delta> {
        if window.recall != RecallSource::Platform {
            return None;
        }
        let MessageId::Native(_) = &self.message.message_id else {
            return None;
        };
        let horizon = self
            .sealed_at
            .map_or(horizon(window), |at| horizon(window).max(at));
        let (mut messages, names) = self
            .platform(window, Some(&watermark.after), |message| {
                message.at >= horizon && !message.from_bot && !watermark.taken.contains(&message.id)
            })
            .await?;
        let mut spent = 0_usize;
        let keep = messages
            .iter()
            .rev()
            .take_while(|message| {
                spent += names.label(message).len() + message.text.len() + 1;
                spent <= window.limits.max_bytes
            })
            .count();
        messages.drain(..messages.len() - keep);
        if messages.is_empty() {
            return None;
        }
        tracing::Span::current().record("conversation.recall_source", "platform");
        tracing::info!(
            event = "gateway_recalled",
            source = "platform",
            messages = messages.len(),
            delta = messages.len()
        );
        Some(Delta { messages, names })
    }

    async fn platform(
        &self,
        window: MemoryWindow,
        after: Option<&str>,
        seeds: impl Fn(&PastMessage) -> bool,
    ) -> Option<(Vec<PastMessage>, Names)> {
        let history = self.driver.history()?;
        let deadline = tokio::time::Instant::now() + PLATFORM_RECALL_TIMEOUT;
        let limit = window
            .limits
            .max_turns
            .saturating_mul(2)
            .min(PLATFORM_RECALL_MAX_MESSAGES);
        let before = match &self.message.message_id {
            MessageId::Native(id) => Some(id.as_str()),
            MessageId::Wake { .. } | MessageId::Job { .. } => None,
        };
        let read = tokio::time::timeout_at(
            deadline,
            history.recent(&self.message.conversation, after, before, limit),
        )
        .await;
        let reason = match read {
            Ok(Ok(mut messages)) => {
                messages.retain(seeds);
                let names = self.names(&messages, window.scope, deadline).await;
                return Some((messages, names));
            }
            Ok(Err(error)) => error.category(),
            Err(_) => "timeout",
        };
        tracing::warn!(event = "gateway_recall_failed", source = "platform", reason);
        None
    }

    async fn names(
        &self,
        messages: &[PastMessage],
        scope: MemoryScope,
        deadline: tokio::time::Instant,
    ) -> Names {
        let mut principals = HashMap::new();
        if let Some(sender) = self.sender {
            principals.insert(self.message.subject.clone(), sender.clone());
        }
        let claims = messages
            .iter()
            .filter(|message| !message.from_bot && message.subject != self.message.subject)
            .map(|message| &message.subject)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|subject| chat_claim_for(self.route, self.message, subject.clone()).ok())
            .collect::<Vec<_>>();
        if !claims.is_empty()
            && let Ok(client) = self.runner.broker.leg_client()
        {
            let client = &client;
            let lookups = futures_util::stream::iter(claims)
                .map(|claim| async move {
                    let subject = claim.subject.clone();
                    let surface = client.session_surface(Some(claim)).await.ok()?;
                    Some((subject, surface.principal?))
                })
                .buffer_unordered(PRINCIPAL_LOOKUPS_IN_FLIGHT)
                .for_each(|named| {
                    if let Some((subject, principal)) = named {
                        principals.insert(subject, principal);
                    }
                    std::future::ready(())
                });
            tokio::time::timeout_at(deadline, lookups)
                .await
                .unwrap_or_default();
        }
        Names { principals, scope }
    }
}

fn horizon(window: MemoryWindow) -> SystemTime {
    SystemTime::now()
        .checked_sub(window.forget_after)
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn platform_window(
    messages: Vec<PastMessage>,
    names: &Names,
    window: MemoryWindow,
) -> RecalledWindow {
    let newest = messages.last().map(|message| message.id.clone());
    let mut turns = Vec::new();
    let mut assets = Vec::new();
    let mut pending_user = String::new();
    let mut next_asset_id = 1;
    for message in messages {
        let label = names.label(&message);
        let mut text = message.text;
        for asset in message.assets {
            text.push_str(&asset::attached_marker(next_asset_id, &asset.name));
            assets.push(RecalledAsset {
                id: next_asset_id,
                asset,
                arrived: message.at,
            });
            next_asset_id += 1;
        }
        if message.from_bot {
            let user = if pending_user.is_empty() {
                "[gateway: earlier in this conversation]".to_owned()
            } else {
                std::mem::take(&mut pending_user)
            };
            turns.push(ConversationTurn::completed(user, text));
        } else {
            if !pending_user.is_empty() {
                pending_user.push('\n');
            }
            pending_user.push_str(&format!("{label}\n{text}"));
        }
    }
    if !pending_user.is_empty() {
        turns.push(ConversationTurn::unanswered(pending_user));
    }
    let excess = assets
        .len()
        .saturating_sub(asset::MAX_ASSETS_PER_CONVERSATION);
    assets.drain(..excess);
    RecalledWindow {
        history: History::from_turns(window.limits, turns),
        assets,
        next_asset_id,
        newest,
    }
}

impl Delta {
    fn block(
        self,
        store: &AssetStore,
        access: &AssetAccess,
        images_supported: bool,
    ) -> Option<String> {
        if self.messages.is_empty() {
            return None;
        }
        let arriving = self
            .messages
            .iter()
            .flat_map(|message| message.assets.iter().cloned())
            .collect();
        let registered =
            store.assets_for_access(access, arriving, images_supported, Instant::now());
        let mut ids = registered.arrived.into_iter();
        let mut block = String::new();
        for message in &self.messages {
            if !block.is_empty() {
                block.push('\n');
            }
            block.push_str(&format!("{}\n{}", self.names.label(message), message.text));
            for (asset, id) in message.assets.iter().zip(ids.by_ref()) {
                block.push_str(&asset::attached_marker(id, &asset.name));
            }
        }
        Some(block)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "reshaped by the unit that next rewrites this"
)]
async fn append_journal(
    journal: &Arc<Journal>,
    key: &ConversationKey,
    granted: &[String],
    window: MemoryWindow,
    turn: ConversationTurn,
    inventory: Vec<asset::AssetRef>,
    next_asset_id: u64,
) {
    let journal = Arc::clone(journal);
    let stem = key.journal_stem();
    let grant = journal::grant_digest(granted);
    let appended = tokio::task::spawn_blocking(move || {
        journal.append(
            &stem,
            &journal::Entry {
                at: SystemTime::now(),
                grant: &grant,
                turn: &turn,
                inventory: &inventory,
                next_asset_id,
            },
            window,
        )
    })
    .await;
    let reason = match appended {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.label(),
        Err(_) => "task",
    };
    tracing::warn!(event = "gateway_journal_append_failed", reason);
}

fn conversation_key(route: &BoundRoute, message: &InboundMessage) -> ConversationKey {
    let conversation = message.conversation.key();
    match route.memory {
        MemoryPolicy::Persistent(MemoryWindow {
            scope: MemoryScope::SharedConversation,
            ..
        }) => ConversationKey::shared(&route.agent, &route.transport, &conversation),
        MemoryPolicy::OneShot
        | MemoryPolicy::Persistent(MemoryWindow {
            scope: MemoryScope::PrivateConversation,
            ..
        }) => ConversationKey::private(
            &route.agent,
            &route.transport,
            &conversation,
            &message.subject,
        ),
    }
}

pub(crate) const RESTRICTED_REPLY_ASSETS_NOTE: &str = "[gateway: this chat shows only image/png and \
image/jpeg; convert other images before `asset send`.]";

fn limits_line(
    limits: dekopon_agent::prompt::PromptLimits,
    max_duration: Option<Duration>,
) -> String {
    let steps = counted(limits.max_steps.into(), "step", "steps");
    let calls = counted(
        limits.max_capability_calls.into(),
        "capability call",
        "capability calls",
    );
    match max_duration {
        Some(duration) if duration.as_secs() % 60 == 0 => format!(
            "[gateway: {steps}, {calls} and {} per message.]",
            counted(duration.as_secs() / 60, "minute", "minutes")
        ),
        Some(duration) => format!(
            "[gateway: {steps}, {calls} and {} per message.]",
            counted(duration.as_secs(), "second", "seconds")
        ),
        None => format!("[gateway: {steps} and {calls} per message.]"),
    }
}

fn counted(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// Only this first line is attribution the gateway vouches for, naming the broker principal the
/// attested sender maps to; the rest is untrusted user text that can contain lookalike labels or
/// prompt injection, even under the local-dev transport.
fn attributed_prompt(principal: Option<&PrincipalId>, text: &str) -> String {
    bound_inbound(&match principal {
        Some(principal) => format!("[gateway: authenticated participant: {principal}]\n{text}"),
        None => format!("[gateway: unmapped participant]\n{text}"),
    })
}

fn message_span(
    route: &BoundRoute,
    message: &InboundMessage,
    receipts: &Dispositions,
) -> tracing::Span {
    let span = tracing::info_span!(
        parent: &message.receive_span,
        "gateway.message",
        transport = %message.transport,
        agent = %route.agent,
        outcome = tracing::field::Empty,
        busy.cause = tracing::field::Empty,
        batch.members = receipts.0.len(),
    );
    for receipt in &receipts.0 {
        dekopon_telemetry::link_span(&span, receipt);
    }
    span
}

pub(crate) fn run_session(
    runner: Arc<SessionRunner>,
    route: BoundRoute,
    message: InboundMessage,
    driver: Arc<dyn ChatDriver>,
) -> impl std::future::Future<Output = ()> + Send {
    let receipts = Dispositions(message.constituents.clone());
    async move {
        let span = message_span(&route, &message, &receipts);
        let mut admission = execute(&runner, &route, message, &driver, receipts)
            .instrument(span)
            .await;
        while let Some((next, followup)) = admission.and_then(SessionAdmission::next_or_release) {
            let span = message_span(&followup.route, &followup.message, &followup.receipts);
            admission = Some(
                run_admitted(
                    &runner,
                    &followup.route,
                    followup.message,
                    &driver,
                    followup.receipts,
                    next,
                )
                .instrument(span)
                .await,
            );
        }
    }
}

async fn execute(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    receipts: Dispositions,
) -> Option<SessionAdmission> {
    if message.constituents.is_empty() {
        crate::collection::record_received(&message);
    }
    let liveness = message.liveness.clone();
    let is_unattended = match message.message_id {
        MessageId::Native(_) => false,
        MessageId::Wake { .. } | MessageId::Job { .. } => true,
    };
    let notice = crate::jobs::Notice::of(&message);
    match runner.gate.admit(route, message, receipts) {
        Admit::Admitted(admission, message, receipts) => {
            if let Some(notice) = &notice {
                notice.record("new-turn");
            }
            Some(run_admitted(runner, route, message, driver, receipts, admission).await)
        }
        Admit::Steered(receipts) => {
            if let Some(notice) = &notice {
                notice.record("steered");
            }
            tracing::Span::current().record("outcome", "steered");
            receipts.finish("steered");
            acknowledge_steer(driver.as_ref(), liveness.as_ref(), &route.transport).await;
            None
        }
        Admit::Queued => {
            if let Some(notice) = &notice {
                notice.record("queued");
            }
            tracing::Span::current().record("outcome", "queued");
            if !is_unattended {
                acknowledge_steer(driver.as_ref(), liveness.as_ref(), &route.transport).await;
            }
            None
        }
        Admit::Full(message, receipts) | Admit::Saturated(message, receipts) => {
            if let Some(notice) = &notice {
                notice.record("dropped");
            }
            tracing::info!(event = "gateway_session_rejected", reason = "busy");
            if notice.is_none()
                && (runner.reply_on_busy || !message.constituents.is_empty())
                && let Some(_reply) = runner.gate.refusal()
            {
                answer(driver, &message, BUSY_REPLY).await;
            }
            tracing::Span::current().record("outcome", "busy");
            receipts.finish("busy");
            None
        }
    }
}

async fn acknowledge_steer(
    driver: &dyn ChatDriver,
    target: Option<&crate::transport::LivenessTarget>,
    transport: &str,
) {
    if let (Some(ack), Some(target)) = (driver.steer_ack(), target)
        && let Err(error) = crate::progress::bounded(ack.seen(target)).await
    {
        tracing::debug!(event = "gateway_steer_ack_failed", transport, error = %error);
    }
}

async fn run_admitted(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    receipts: Dispositions,
    admission: SessionAdmission,
) -> SessionAdmission {
    let outcome = session(runner, route, &message, driver, admission.cancellation())
        .instrument(tracing::info_span!(
            "gateway.session",
            agent = %route.agent,
            gen_ai.agent.name = %route.agent,
            gen_ai.operation.name = "invoke_agent",
            conversation.kind = message.conversation.kind.as_str(),
            conversation.container = message.conversation.container.as_deref().unwrap_or_default(),
            conversation.id = message.conversation.id.as_str(),
            conversation.thread = message.conversation.thread.as_deref().unwrap_or_default(),
            conversation.turns = tracing::field::Empty,
            conversation.bytes = tracing::field::Empty,
            conversation.recall_source = tracing::field::Empty,
            conversation.recalled_turns = tracing::field::Empty,
            conversation.carried_assets = tracing::field::Empty,
        ))
        .await;
    tracing::Span::current().record("outcome", outcome);
    receipts.finish(outcome);
    admission
}

struct SessionSteers {
    gate: SessionGate,
    key: AdmissionKey,
    store: Arc<AssetStore>,
    access: AssetAccess,
    images_supported: bool,
    scope: Option<MemoryScope>,
    principal: Option<PrincipalId>,
    assets: Arc<SessionAssets>,
    received_at: tokio::time::Instant,
    raw_texts: Mutex<Vec<String>>,
    taken: Mutex<Vec<String>>,
}

impl SteerSource for SessionSteers {
    fn drain(&self) -> Vec<Steer> {
        self.gate
            .take_steers(&self.key)
            .into_iter()
            .map(|steer| {
                match &steer.message_id {
                    MessageId::Job { .. } => return Steer::Notice(steer.text),
                    MessageId::Native(id) => self.taken.lock().push(id.clone()),
                    MessageId::Wake { .. } => {}
                }
                let seconds = steer
                    .received_at
                    .saturating_duration_since(self.received_at)
                    .as_secs();
                let text = bound_inbound(&format!(
                    "[gateway: sent while you were working, +{seconds}s]\n{}",
                    steer.text
                ));
                let registered = self.store.assets_for_access(
                    &self.access,
                    steer.assets,
                    self.images_supported,
                    Instant::now(),
                );
                self.assets.arrived(registered.fetchable);
                let mut recorded =
                    bound_inbound(&format!("{text}{}", asset::arrival_markers(&registered)));
                let mut prompt = match asset::reference_note(&registered, self.images_supported) {
                    Some(note) => bound_inbound(&format!("{text}\n\n{note}")),
                    None => text,
                };
                if self.scope == Some(MemoryScope::SharedConversation) {
                    prompt = attributed_prompt(self.principal.as_ref(), &prompt);
                    recorded = attributed_prompt(self.principal.as_ref(), &recorded);
                }
                self.raw_texts.lock().push(steer.text);
                Steer::Person { prompt, recorded }
            })
            .collect()
    }
}

impl SessionSteers {
    fn user_text(&self, prompt: &str) -> String {
        let mut text = prompt.to_owned();
        for steer in self.raw_texts.lock().iter() {
            text.push_str("\n\n");
            text.push_str(steer);
        }
        text
    }
}

#[expect(
    clippy::too_many_lines,
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
async fn session(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: &InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    cancellation: SessionCancellation,
) -> &'static str {
    let key = conversation_key(route, message);
    let sealed = sealed_conversation(runner, &key).await;
    match message.conversation.kind {
        ConversationKind::Thread if sealed.is_some() => {
            tracing::info!(event = "gateway_session_rejected", reason = "sealed");
            answer(driver, message, SEALED_THREAD_REPLY).await;
            return "sealed";
        }
        ConversationKind::Thread
        | ConversationKind::DirectMessage
        | ConversationKind::GroupDirectMessage
        | ConversationKind::Channel => {}
    }
    let leg = match connect(runner, route, message).await {
        Ok(leg) => leg,
        Err(SessionError::BrokerLeg(BrokerLegError::Client(ClientError::Remote {
            code, ..
        }))) if code == ERROR_UNAUTHENTICATED => {
            revoke_thread_ownership(runner, message);
            tracing::info!(
                event = "gateway_session_rejected",
                reason = "attestation-refused"
            );
            answer(driver, message, UNAUTHORIZED_REPLY).await;
            return "unauthorized";
        }
        Err(error) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category(),
                error = %error
            );
            answer(driver, message, FAILURE_REPLY).await;
            return "failed";
        }
    };
    // Same-sender steers use this leg; other senders and wakes open their own follow-up leg.
    let granted = leg.granted();
    // Removing the entry, not just refusing further calls, matters because a revoked subject's
    // exchange left resident for its idle timeout would hold exactly the text the revocation was
    // about.
    if granted.is_empty() {
        revoke_thread_ownership(runner, message);
        runner
            .conversations
            .remove(&key, EvictionReason::GrantChanged);
        tracing::info!(event = "gateway_session_rejected", reason = "unauthorized");
        answer(driver, message, UNAUTHORIZED_REPLY).await;
        return "unauthorized";
    }
    // A chat thread becomes a continuation surface only after this exact sender's fresh broker
    // grant succeeds; merely mentioning the bot or putting coordinates in model text cannot claim
    // one.
    claim_thread_ownership(runner, message);
    let agent_config = agent_config_view(
        route.agent.as_str(),
        &route.description,
        route.model_class.as_deref(),
        route.instructions.as_deref(),
        &route.skills,
        route.limits,
        route.memory,
        &leg,
    );

    let window = route.memory.window();
    let principal = leg.principal().cloned();
    let recall = Recall {
        runner,
        route,
        message,
        driver: driver.as_ref(),
        sender: principal.as_ref(),
        sealed_at: sealed.map(|seal| seal.at),
    };
    let mut newest_seen = match &message.message_id {
        MessageId::Native(id) => Some(id.clone()),
        MessageId::Wake { .. } | MessageId::Job { .. } => None,
    };
    let (mut seeded, cache_key, conversation_lease, asset_access, delta) = match window {
        Some(window) => {
            let (recalled, delta) =
                match runner
                    .conversations
                    .resident(&key, &granted, window, Instant::now())
                {
                    Residency::Cold => (recall.window(&key, &granted, window).await, None),
                    Residency::Since(watermark) => (None, recall.delta(window, &watermark).await),
                    Residency::Resident => (None, None),
                };
            let (recalled_history, recalled_assets, next_asset_id) = match recalled {
                Some(recalled) => {
                    newest_seen = newest_seen.or(recalled.newest);
                    (
                        Some(recalled.history),
                        recalled.assets,
                        recalled.next_asset_id,
                    )
                }
                None => (None, Vec::new(), 1),
            };
            let ConversationSeed {
                history,
                cache_key,
                assets,
                lease,
                created,
            } = runner.conversations.begin(
                &key,
                &granted,
                window,
                recalled_history,
                Instant::now(),
            );
            if created {
                let span = tracing::Span::current();
                span.record("conversation.recalled_turns", history.len());
                span.record("conversation.carried_assets", recalled_assets.len());
                runner
                    .assets
                    .restore(&assets, recalled_assets, next_asset_id, Instant::now());
            }
            (history, cache_key, Some(lease), assets, delta)
        }
        None => (
            History::default(),
            route.cache_key.clone(),
            None,
            AssetAccess::one_shot(key.clone()),
            None,
        ),
    };
    let span = tracing::Span::current();
    span.record("conversation.turns", seeded.len());
    span.record("conversation.bytes", seeded.bytes());
    tracing::info!(
        target: "dekopon_gatewayd::audit",
        {
            audit.event = "gateway.session.cache_key",
            prompt.cache_key = cache_key.as_str(),
            conversation.persistent = window.is_some(),
        },
        "gateway session prompt cache key"
    );

    let memory_surface = leg.chat_memory_surface().cloned();
    let chat_claim = chat_claim(route, message).ok();
    let model_config = Arc::clone(&route.model);
    let models = Arc::clone(&runner.models);
    let limits = route.limits;
    let assets_note = match &message.reply {
        crate::transport::ReplyTarget::Telegram { .. }
        | crate::transport::ReplyTarget::WhatsApp { .. } => Some(RESTRICTED_REPLY_ASSETS_NOTE),
        _ => None,
    };
    let instructions = [
        route.instructions.as_deref(),
        memory_surface
            .as_ref()
            .map(|memory| memory.prompt_note.as_str()),
        assets_note,
        Some(limits_line(limits, route.max_duration).as_str()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n");
    let images_supported = route.model.accepts_images();
    let delta = delta
        .and_then(|delta| delta.block(&runner.assets, &asset_access, images_supported))
        .map(ConversationTurn::unanswered);
    if let Some(delta) = &delta {
        seeded.record(delta.clone());
    }
    let registered = runner.assets.assets_for_access(
        &asset_access,
        message.assets.clone(),
        images_supported,
        Instant::now(),
    );
    let text = match asset::reference_note(&registered, images_supported) {
        Some(note) if message.text.trim().is_empty() => note,
        Some(note) => bound_inbound(&format!("{}\n\n{note}", message.text)),
        None => message.text.clone(),
    };
    let recorded = bound_inbound(&format!(
        "{}{}",
        message.text,
        asset::arrival_markers(&registered)
    ));
    let journal_access = asset_access.clone();
    let delivery_notice = runner.assets.take_delivery_notice(&asset_access);
    let stop_notice = runner.pending_notices.lock().remove(&key);
    let shared = window.map(|window| window.scope) == Some(MemoryScope::SharedConversation);
    let frame = |text: String| {
        let text = match &delivery_notice {
            Some(note) => bound_inbound(&format!("{note}\n{text}")),
            None => text,
        };
        if shared {
            attributed_prompt(principal.as_ref(), &text)
        } else {
            text
        }
    };
    let text = match stop_notice {
        Some(PendingNotice(cause)) => bound_inbound(&format!("{}\n{text}", cause.notice())),
        None => text,
    };
    let text = frame(text);
    let recorded = frame(recorded);
    let assets = Arc::new(SessionAssets::new(
        Arc::clone(&runner.assets),
        asset_access.clone(),
        runner.asset_fetchers.get(&message.transport).cloned(),
        tokio::runtime::Handle::current(),
        images_supported,
        registered.fetchable,
    ));
    let steers = Arc::new(SessionSteers {
        gate: runner.gate.clone(),
        key: (message.transport.clone(), message.conversation.key()),
        store: Arc::clone(&runner.assets),
        access: asset_access,
        images_supported,
        scope: window.map(|window| window.scope),
        principal: principal.clone(),
        assets: Arc::clone(&assets),
        received_at: message.received_at,
        raw_texts: Mutex::new(Vec::new()),
        taken: Mutex::new(message.folded.clone()),
    });
    let session_steers = Arc::clone(&steers);
    let shell = ShellLimits {
        max_capability_calls: limits.max_capability_calls,
        timeout: route.script_timeout,
        ..ShellLimits::default()
    };
    // Built here per request rather than passed to the shared factory, so a cached client's cache
    // key never gets baked in from the first conversation and silently mislabels every later one.
    let options = CompletionOptions::default().with_prompt_cache_key(cache_key.clone());

    let liveness = runner
        .liveness
        .get(&message.transport)
        .cloned()
        .unwrap_or_default();
    let leg = leg.with_cancel_signal(cancellation.signal());
    let attachments = Arc::new(ReplyAttachments::new(
        asset::MAX_SENDS_PER_TURN,
        Arc::clone(&assets) as Arc<dyn dekopon_agent::attachment::GeneratedAssetStore>,
        message.transport.to_string(),
    ));
    let leg = leg
        .with_provider_attachments(Arc::clone(&attachments))
        .with_chat_asset_inputs(ChatAssetInputs::new(
            Arc::clone(&assets) as Arc<dyn dekopon_agent::attachment::ChatAssetSource>
        ));
    let (settings, keep_alive) = liveness.for_kind(message.conversation.kind);
    // Keep this declared before progress and sink: Rust drops them first, so cancellation can't
    // wake ahead of the terminal reply being sent.
    let _cancel_on_drop = CancellationOnDrop(cancellation.clone());
    let (mut progress, sink) = ProgressPolicy::start(ProgressInputs {
        driver: Arc::clone(driver),
        target: message.liveness.clone(),
        reply: message.reply.clone(),
        transport: message.transport.clone(),
        detail: route.progress_detail,
        liveness: Arc::clone(&liveness),
        settings,
        keep_alive,
        cancellation: cancellation.clone(),
        max_duration: route.max_duration,
    });
    sink.emit(ProgressEvent::Started {
        agent: route.agent.to_string(),
        max_steps: limits.max_steps,
    });
    // The prompt loop and interpreter are synchronous and can block for a long time; running that
    // on a runtime worker would stall every other session in the process.
    let blocking_span = span.clone();
    let prompt_cancellation = cancellation.clone();
    let reply_optional = message
        .thread_continuation
        .as_ref()
        .is_some_and(|continuation| continuation.inherited);
    let skills = Arc::clone(&route.skills);
    let agent = route.agent.to_string();
    let agent_id = route.agent.clone();
    let metering = Arc::clone(&runner.metering);
    let progress_notes = route.progress_notes;
    let inspect_agent_config = route.inspect_agent_config;
    let session_attachments = Arc::clone(&attachments);
    let progress_sink = Arc::clone(&sink) as Arc<dyn ProgressSink>;
    let calls = CallBudget::new(limits.max_capability_calls);
    let mut leg = leg.with_progress(Arc::clone(&progress_sink), calls.clone());
    if progress_notes {
        leg = leg.with_progress_notes();
    }
    if let Some((timeout, anchor)) = route
        .job_timeout
        .zip(Anchor::for_job(message, &route.agent))
    {
        leg = leg.with_job_control(Arc::new(JobContext::for_turn(
            Arc::clone(&runner.jobs),
            message,
            anchor,
            runner.broker.clone(),
            ShellLimits {
                max_capability_calls: limits.max_capability_calls,
                timeout,
                ..ShellLimits::default()
            },
        )));
    }
    drop(sink);
    let wakes = route
        .wakes
        .then_some(runner.wakes.as_ref())
        .flatten()
        .zip(Anchor::from_inbound(message, &route.agent))
        .map(|(store, anchor)| {
            SessionWakes::new(
                anchor,
                Arc::clone(store),
                runner.broker.clone(),
                tokio::runtime::Handle::current(),
                shell,
                route.job_timeout.map(|timeout| {
                    (
                        Arc::clone(&runner.jobs),
                        ShellLimits {
                            max_capability_calls: limits.max_capability_calls,
                            timeout,
                            ..ShellLimits::default()
                        },
                    )
                }),
            )
        });
    let model_runtime = tokio::runtime::Handle::current();
    let model_cancel = cancellation.model_watch();
    let subscriber = tracing::dispatcher::get_default(Clone::clone);
    let result = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&subscriber, || {
            let _entered = blocking_span.enter();
            let model = match models.client(&model_config, model_runtime, model_cancel) {
                Ok(model) => ChatProviderGovernor::wrap(model, &metering, &agent_id, &model_config),
                Err(error) => return (Err(error), None, Vec::new()),
            };
            let runtime = ShellRuntime {
                invoker: leg,
                limits: shell,
                calls,
            };
            let mut history = seeded;
            let mut inputs = SessionInputs::new(&text, limits)
                .with_recorded_prompt(&recorded)
                .with_system(Some(&instructions))
                .with_skills(&skills)
                .with_agent(&agent)
                .with_options(&options)
                .with_assets(assets.as_ref())
                .with_reply_assets(&session_attachments)
                .with_cancellation(&prompt_cancellation)
                .with_steering(session_steers.as_ref())
                .with_progress(Arc::clone(&progress_sink));
            // Withholding the agent-config view here removes the structured dump but not the underlying
            // instructions, which are still the system prompt, so this is not secrecy from a determined
            // user.
            if inspect_agent_config {
                inputs = inputs.with_agent_config(&agent_config);
            }
            if progress_notes {
                inputs = inputs.with_progress_notes();
            }
            if reply_optional {
                inputs = inputs.with_optional_reply();
            }
            if let Some(wakes) = wakes.as_ref() {
                inputs = inputs.with_wakes(wakes);
            }
            let outcome = run_prompt_session(model.as_ref(), &runtime, inputs, &mut history)
                .map_err(SessionError::from);
            let turn = match &outcome {
                Err(SessionError::Prompt(PromptError::ZeroSteps | PromptError::Cancelled)) => None,
                _ => history.turns().last().cloned(),
            };
            let images = if outcome.is_ok() {
                session_attachments.take()
            } else {
                Vec::new()
            };
            (outcome, turn, images)
        })
    })
    .await;

    let (outcome, turn, images) = match result {
        Ok(session) => session,
        Err(_) => {
            if !cancellation.claim_completion() {
                return stopped(runner, &key, &mut progress, &cancellation).await;
            }
            progress.seal();
            tracing::error!(event = "gateway_session_failed", category = "session-task");
            let replied = progress
                .terminal(remember_stop(runner, &key, StopCause::SessionTask))
                .await;
            return if replied { "failed" } else { "reply-failed" };
        }
    };

    if matches!(&outcome, Err(SessionError::Prompt(PromptError::Cancelled)))
        || cancellation.is_cancelled()
        || !cancellation.claim_completion()
    {
        return stopped(runner, &key, &mut progress, &cancellation).await;
    }

    progress.seal();

    if let Some(window) = window
        && let Some(turn) = turn
        && let Some(lease) = conversation_lease
    {
        let journaled = (window.recall == RecallSource::Journal).then(|| turn.clone());
        let taken = TakenIn {
            newest: newest_seen,
            steers: std::mem::take(&mut *steers.taken.lock()),
            recalled: delta,
        };
        if lease.commit(window, turn, taken, &cache_key, Instant::now())
            && let Some(turn) = journaled
            && let Some(journal) = runner.journal.as_ref()
        {
            let inventory = runner.assets.inventory(&journal_access);
            let next_asset_id = journal_access.next_asset_id();
            append_journal(
                journal,
                &key,
                &granted,
                window,
                turn,
                inventory,
                next_asset_id,
            )
            .await;
        }
    }

    if matches!(
        &outcome,
        Ok(outcome) if outcome.disposition == ReplyDisposition::Suppress
    ) {
        progress.terminal(Terminal::Silent).await;
        return "declined";
    }

    let (terminal, completed_outcome, delivered_answer) = match &outcome {
        Ok(outcome) => {
            let text = bound_outbound(if outcome.answer.is_empty() && images.is_empty() {
                EMPTY_REPLY
            } else {
                outcome.answer.as_str()
            });
            let reply = if images.is_empty() {
                OutboundReply::text(text.clone())
            } else {
                OutboundReply::with_images(text.clone(), images)
            };
            (Terminal::Answered(reply), "answered", Some(text))
        }
        Err(SessionError::Model(InferenceError::OverBudget(refusal)))
        | Err(SessionError::Prompt(PromptError::Model(InferenceError::OverBudget(refusal)))) => {
            tracing::info!(event = "gateway_session_refused", category = "over-budget");
            (
                Terminal::Failed(bound_outbound(&refusal.to_string())),
                "refused",
                None,
            )
        }
        Err(SessionError::Prompt(PromptError::UnreportedCapabilityWork)) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = "unreported-capability-work"
            );
            (
                Terminal::Failed(UNREPORTED_WORK_REPLY.to_owned()),
                "failed",
                None,
            )
        }
        Err(error) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category(),
                error = %error
            );
            let terminal = match error {
                SessionError::Model(error) | SessionError::Prompt(PromptError::Model(error)) => {
                    remember_stop(runner, &key, StopCause::Model(error.kind()))
                }
                SessionError::Prompt(PromptError::EmptyAnswer) => {
                    remember_stop(runner, &key, StopCause::EmptyAnswer)
                }
                SessionError::Prompt(PromptError::MaxSteps { .. }) => {
                    remember_stop(runner, &key, StopCause::MaxSteps)
                }
                _ => Terminal::Failed(bound_outbound(liveness.templates.failed())),
            };
            (terminal, "failed", None)
        }
    };
    let delivered = progress.terminal(terminal).await;
    attachments.finish(match (&outcome, delivered) {
        (Ok(_), true) => AssetDeliveryDisposition::Delivered,
        (Ok(_), false) => AssetDeliveryDisposition::Failed,
        (Err(_), _) => AssetDeliveryDisposition::Abandoned,
    });
    if delivered {
        if memory_surface.is_some()
            && let Some(answer) = delivered_answer
            && let Some(claim) = chat_claim
        {
            record_delivered_turn(
                runner,
                message,
                claim,
                steers.user_text(&message.text),
                DeliveredAnswer::accepted_by_transport(answer),
            )
            .await;
        }
        completed_outcome
    } else {
        "reply-failed"
    }
}

async fn sealed_conversation(
    runner: &SessionRunner,
    key: &ConversationKey,
) -> Option<SealedConversation> {
    let Some(journal) = runner.journal.as_ref() else {
        return runner.sealed_conversations.lock().get(key).copied();
    };
    let journal = Arc::clone(journal);
    let stem = key.journal_stem();
    let result = tokio::task::spawn_blocking(move || journal.sealed(&stem)).await;
    let reason = match result {
        Ok(Ok(sealed)) => return sealed,
        Ok(Err(error)) => error.label(),
        Err(_) => "task",
    };
    tracing::warn!(event = "gateway_recall_failed", source = "seal", reason);
    None
}

async fn seal_conversation(runner: &SessionRunner, key: &ConversationKey) {
    let seal = SealedConversation {
        at: SystemTime::now(),
    };
    let Some(journal) = runner.journal.as_ref() else {
        runner.sealed_conversations.lock().insert(key.clone(), seal);
        return;
    };
    let journal = Arc::clone(journal);
    let stem = key.journal_stem();
    let result = tokio::task::spawn_blocking(move || journal.seal(&stem, seal.at)).await;
    let reason = match result {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error.label(),
        Err(_) => "task",
    };
    tracing::warn!(event = "gateway_journal_seal_failed", reason);
}

fn remember_stop(runner: &SessionRunner, key: &ConversationKey, cause: StopCause) -> Terminal {
    runner
        .pending_notices
        .lock()
        .insert(key.clone(), PendingNotice(cause));
    Terminal::Stopped(cause)
}

async fn stopped(
    runner: &SessionRunner,
    key: &ConversationKey,
    progress: &mut ProgressPolicy,
    cancellation: &SessionCancellation,
) -> &'static str {
    tracing::info!(event = "gateway_session_cancelled");
    let by = cancellation.source().unwrap_or(CancelSource::Operator);
    match by {
        CancelSource::Budget {
            limit: BudgetLimit::WallClock,
        } => {
            seal_conversation(runner, key).await;
            runner.conversations.remove(key, EvictionReason::Sealed);
        }
        CancelSource::User { .. } | CancelSource::Operator => {}
    }
    progress
        .terminal(remember_stop(runner, key, StopCause::Cancelled(by)))
        .await;
    "cancelled"
}

fn claim_thread_ownership(runner: &SessionRunner, message: &InboundMessage) {
    let Some(continuation) = &message.thread_continuation else {
        return;
    };
    if let Some(ownership) = runner.thread_ownership.get(&message.transport) {
        ownership.claim(continuation.claim.clone());
    }
}

fn revoke_thread_ownership(runner: &SessionRunner, message: &InboundMessage) {
    let Some(continuation) = &message.thread_continuation else {
        return;
    };
    if let Some(ownership) = runner.thread_ownership.get(&message.transport) {
        ownership.revoke(&continuation.claim);
    }
}

/// Deliberately takes no model config, broker config, message, subject, or principal: a constructor
/// that cannot receive credentials or identity is stronger than one expected to remember to redact
/// them.
#[allow(
    clippy::too_many_arguments,
    reason = "every argument is one catalog or route fact the view names; bundling them would hide which fact a caller forgot"
)]
fn agent_config_view(
    agent: &str,
    description: &str,
    model_class: Option<&str>,
    instructions: Option<&str>,
    skills: &[dekopon_config::Skill],
    limits: dekopon_agent::prompt::PromptLimits,
    memory: MemoryPolicy,
    leg: &BrokerLeg,
) -> AgentConfigView {
    let memory = match memory {
        MemoryPolicy::OneShot => MemoryConfigView::OneShot,
        MemoryPolicy::Persistent(window) => MemoryConfigView::Persistent {
            scope: match window.scope {
                MemoryScope::PrivateConversation => MemoryScopeView::PrivateConversation,
                MemoryScope::SharedConversation => MemoryScopeView::SharedConversation,
            },
            idle_timeout_ms: u64::try_from(window.idle_timeout.as_millis()).unwrap_or(u64::MAX),
            max_turns: window.limits.max_turns,
            max_bytes: window.limits.max_bytes,
        },
    };
    AgentConfigView::new(
        agent.to_owned(),
        description.to_owned(),
        model_class.map(str::to_owned),
        instructions.map(str::to_owned),
        SessionConfigView {
            max_steps: limits.max_steps,
            max_capability_calls: limits.max_capability_calls,
            memory,
        },
        leg.effective_capabilities(),
    )
    .with_skills(
        skills
            .iter()
            .map(|skill| SkillView {
                name: skill.name().to_string(),
                description: skill.description().to_owned(),
                resources: skill
                    .resources()
                    .iter()
                    .map(|resource| resource.path.clone())
                    .collect(),
            })
            .collect(),
    )
}

async fn connect(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: &InboundMessage,
) -> Result<BrokerLeg, SessionError> {
    let client = runner.broker.leg_client()?;
    BrokerLeg::connect(client, Some(chat_claim(route, message)?))
        .await
        .map_err(SessionError::from)
}

/// No normalization step here: the transport already minted the conversation in the canonical form
/// the grant, claim check, and policy engine all compare against, so re-normalizing would create a
/// second definition of the same fact.
fn chat_claim(route: &BoundRoute, message: &InboundMessage) -> Result<Attestation, SessionError> {
    chat_claim_for(route, message, message.subject.clone())
}

fn chat_claim_for(
    route: &BoundRoute,
    message: &InboundMessage,
    subject: ExternalSubject,
) -> Result<Attestation, SessionError> {
    let transport = message
        .transport
        .parse()
        .map_err(SessionError::TransportId)?;
    Ok(Attestation::for_chat(
        subject,
        route.agent.clone(),
        ChatScopeClaim {
            transport,
            kind: message.transport_kind,
            conversation: message.conversation.clone(),
            trigger: message.message_id.trigger(),
        },
    ))
}

async fn record_delivered_turn(
    runner: &SessionRunner,
    message: &InboundMessage,
    claim: Attestation,
    user: String,
    assistant: DeliveredAnswer,
) {
    let MessageId::Native(message_id) = &message.message_id else {
        tracing::debug!(event = "gateway_memory_record_skipped", reason = "wake");
        return;
    };
    let Some(delivery) = delivery_identity(message, message_id, &claim) else {
        tracing::warn!(
            event = "gateway_memory_record_failed",
            category = "delivery-identity",
        );
        return;
    };
    let result: Result<(), MemoryRecordFailure> = async {
        let identifiers = IdSequence::for_session();
        let client = runner
            .broker
            .leg_client()
            .map_err(|error| MemoryRecordFailure::Broker(BrokerLegError::from(error)))?;
        let result = client
            .record_delivered_turn(
                claim,
                DeliveredTurnRequest::new(
                    identifiers.next_invocation(),
                    identifiers.trace_parent(),
                    delivery,
                    user,
                    assistant,
                ),
            )
            .await
            .map_err(|error| MemoryRecordFailure::Broker(BrokerLegError::from(error)))?;
        memory_record_outcome_category(&result).map_or(Ok(()), |category| {
            Err(MemoryRecordFailure::Outcome(category))
        })
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(
            event = "gateway_memory_record_failed",
            category = memory_record_category(&error),
        );
    }
}

pub(crate) fn delivery_identity(
    message: &InboundMessage,
    message_id: &str,
    claim: &Attestation,
) -> Option<DeliveryIdentity> {
    let scope = claim.scope.as_ref()?;
    let conversation = &scope.conversation;
    match message.transport_kind {
        dekopon_broker_protocol::ChatTransportKind::Slack => Some(DeliveryIdentity::Slack {
            channel: conversation.id.clone(),
            timestamp: message_id.to_owned(),
        }),
        dekopon_broker_protocol::ChatTransportKind::Discord => Some(DeliveryIdentity::Discord {
            channel: conversation
                .api_channel(dekopon_broker_protocol::ChatTransportKind::Discord)
                .to_owned(),
            message: message_id.to_owned(),
        }),
        dekopon_broker_protocol::ChatTransportKind::Telegram => Some(DeliveryIdentity::Telegram {
            chat: conversation.id.clone(),
            topic: conversation.thread.clone(),
            message: message_id.to_owned(),
        }),
        dekopon_broker_protocol::ChatTransportKind::Whatsapp => {
            let container = conversation.container.as_deref()?;
            let (waba, phone_number) = container.split_once(':')?;
            if phone_number.contains(':') {
                return None;
            }
            Some(DeliveryIdentity::Whatsapp {
                waba: waba.to_owned(),
                phone_number: phone_number.to_owned(),
                message: message_id.to_owned(),
            })
        }
        dekopon_broker_protocol::ChatTransportKind::Local => {
            let mut fields = message_id.rsplitn(3, '-');
            let sequence = fields.next()?.parse().ok()?;
            let connection = fields.next()?.parse().ok()?;
            let boot_nonce = fields.next()?.to_owned();
            Some(DeliveryIdentity::Local {
                transport: scope.transport.clone(),
                conversation: conversation.key(),
                boot_nonce,
                connection,
                sequence,
            })
        }
    }
}

pub(crate) fn memory_record_outcome_category(result: &InvocationResult) -> Option<&'static str> {
    match result.outcome {
        InvocationOutcome::Succeeded => None,
        InvocationOutcome::Denied => Some("denied"),
        InvocationOutcome::Failed => Some(match result.error.as_deref() {
            Some("memory-corrupt") => "memory-corrupt",
            Some("result-too-large") => "result-too-large",
            Some("storage-quota") => "storage-quota",
            Some("storage-busy") => "storage-busy",
            Some("storage-timeout") => "storage-timeout",
            Some("storage-corrupt") => "storage-corrupt",
            Some("storage-io") => "storage-io",
            // Never copy a future provider or public error into telemetry; an explicit allowlist,
            // not a passthrough, is what keeps this category stable and content-free by
            // construction.
            _ => "failed",
        }),
    }
}

enum MemoryRecordFailure {
    Broker(BrokerLegError),
    Outcome(&'static str),
}

fn memory_record_category(error: &MemoryRecordFailure) -> &'static str {
    match error {
        MemoryRecordFailure::Broker(BrokerLegError::Client(ClientError::Remote {
            code, ..
        })) if code == "outcome-unaudited" => "outcome-unaudited",
        MemoryRecordFailure::Broker(BrokerLegError::Client(ClientError::Remote {
            code, ..
        })) if code == ERROR_UNAUTHENTICATED => "denied",
        MemoryRecordFailure::Broker(BrokerLegError::Client(ClientError::Remote {
            code, ..
        })) if code == ERROR_STORAGE_QUOTA => ERROR_STORAGE_QUOTA,
        MemoryRecordFailure::Broker(BrokerLegError::Client(ClientError::Remote {
            code, ..
        })) if code == ERROR_STORAGE_BUSY => ERROR_STORAGE_BUSY,
        MemoryRecordFailure::Broker(BrokerLegError::Client(ClientError::Remote {
            code, ..
        })) if code == ERROR_STORAGE_TIMEOUT => ERROR_STORAGE_TIMEOUT,
        MemoryRecordFailure::Broker(BrokerLegError::Client(ClientError::Remote {
            code, ..
        })) if code == ERROR_STORAGE_CORRUPT => ERROR_STORAGE_CORRUPT,
        MemoryRecordFailure::Broker(BrokerLegError::Client(ClientError::Remote {
            code, ..
        })) if code == ERROR_STORAGE_IO => ERROR_STORAGE_IO,
        MemoryRecordFailure::Broker(BrokerLegError::Client(_)) => "broker",
        MemoryRecordFailure::Broker(BrokerLegError::DuplicateCapabilities { .. }) => {
            "duplicate-capability"
        }
        MemoryRecordFailure::Outcome(category) => category,
    }
}

pub(crate) async fn answer(
    driver: &Arc<dyn ChatDriver>,
    message: &InboundMessage,
    text: &str,
) -> bool {
    match driver
        .reply(&message.reply, OutboundReply::text(bound_outbound(text)))
        .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(event = "gateway_reply_failed", category = error.category());
            false
        }
    }
}

#[derive(Debug, Error)]
#[error("model {model:?} credential environment variable {variable} is unusable")]
pub struct ModelCredentialError {
    pub model: String,
    pub variable: String,
    #[source]
    source: TransportError,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("broker client could not be created")]
    BrokerClient(#[from] ClientError),
    #[error("broker leg could not be opened")]
    BrokerLeg(#[from] BrokerLegError),
    #[error("configured chat transport identifier is invalid")]
    TransportId(#[source] dekopon_core::IdentifierError),
    #[error(transparent)]
    Model(#[from] InferenceError),
    #[error(transparent)]
    ModelCredential(#[from] ModelCredentialError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("model {model:?} is served only through the guest model proxy")]
    ProxyOnlyModel { model: String },
}

impl SessionError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::BrokerClient(_) => "broker-client",
            Self::BrokerLeg(_) => "broker-leg",
            Self::TransportId(_) => "transport-id",
            Self::Model(error) | Self::Prompt(PromptError::Model(error)) => error.kind().as_str(),
            Self::ModelCredential(_) => "model-credential",
            Self::ProxyOnlyModel { .. } => "proxy-only-model",
            Self::Prompt(error) => error.telemetry_kind(),
        }
    }
}

#[cfg(test)]
mod model_factory_tests {
    use super::*;

    #[test]
    fn the_real_factory_constructs_openrouter_and_preserves_its_authored_controls() {
        let model: ModelConfig = serde_json::from_value(serde_json::json!({"kind":"openrouter", "name":"router", "model":"vendor/model", "apiKeyEnv":"OPENROUTER_API_KEY", "timeoutMs":2000, "generation":{"maxOutputTokens":1}})).unwrap();
        let factory = ConfiguredModels::default();
        let built = factory
            .construct_with(&model, |variable| {
                assert_eq!(variable, "OPENROUTER_API_KEY");
                Some("synthetic-key".into())
            })
            .unwrap();
        assert!(matches!(built.as_ref(), ModelClient::OpenRouter(_)));
        assert!(matches!(
            factory.construct_with(&model, |_| None),
            Err(SessionError::ModelCredential(_))
        ));
        let mut invalid = model;
        if let ModelConfig::Openrouter {
            generation: Some(generation),
            ..
        } = &mut invalid
        {
            generation.temperature = Some(f64::NAN);
        }
        assert!(matches!(
            factory.construct_with(&invalid, |_| Some("synthetic-key".into())),
            Err(SessionError::Model(InferenceError::InvalidRequest(
                dekopon_model::error::RequestError::OpenRouterSetting(_)
            )))
        ));
    }

    fn compatible(name: &str) -> ModelConfig {
        serde_json::from_value(serde_json::json!({"kind":"openaiCompatible", "name":name, "endpoint":"http://127.0.0.1:9", "model":"fixture", "timeoutMs":2000})).unwrap()
    }

    #[test]
    fn the_real_factory_shares_pools_by_configured_name_only() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let factory = ConfiguredModels::default();
        let bind = |name| {
            factory
                .build(
                    &compatible(name),
                    runtime.handle().clone(),
                    tokio::sync::watch::channel(false).1,
                )
                .unwrap()
        };
        let first = bind("one");
        let first_pool = Arc::clone(factory.clients.lock().get("one").unwrap());
        let again = bind("one");
        let other = bind("two");
        assert!(!Arc::ptr_eq(&first, &again));
        let clients = factory.clients.lock();
        assert!(Arc::ptr_eq(&first_pool, clients.get("one").unwrap()));
        assert!(!Arc::ptr_eq(&first_pool, clients.get("two").unwrap()));
        drop(clients);
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(factory.clients.lock().len(), 2);
    }

    #[test]
    fn a_failed_client_build_is_not_cached_and_codex_bridges_are_per_session() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let factory = ConfiguredModels::default();
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("auth.json");
        let config: ModelConfig = serde_json::from_value(serde_json::json!({"kind":"chatgptSubscription", "name":"codex", "model":"fixture", "timeoutMs":2000, "authFile":path})).unwrap();
        let bind = || {
            factory.build(
                &config,
                runtime.handle().clone(),
                tokio::sync::watch::channel(false).1,
            )
        };
        assert!(matches!(
            bind(),
            Err(SessionError::Model(InferenceError::Authentication(_)))
        ));
        assert!(factory.clients.lock().is_empty());
        std::fs::write(path, serde_json::to_vec(&serde_json::json!({"version":1,"access":"synthetic","refresh":"synthetic","expiresAt":u64::MAX,"accountId":"synthetic"})).unwrap()).unwrap();
        let first = bind().unwrap();
        let again = bind().unwrap();
        assert!(!Arc::ptr_eq(&first, &again));
        let cache = factory.clients.lock();
        assert_eq!(cache.len(), 1);
        assert!(matches!(
            cache.get("codex"),
            Some(client) if matches!(client.as_ref(), ModelClient::Codex(_))
        ));
    }

    #[test]
    fn chatgpt_models_on_one_auth_file_share_one_credential() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let factory = ConfiguredModels::default();
        let directory = tempfile::TempDir::new().unwrap();
        let credential = serde_json::json!({"version":1,"access":"synthetic","refresh":"synthetic","expiresAt":u64::MAX,"accountId":"synthetic"});
        let shared = directory.path().join("auth.json");
        let separate = directory.path().join("other.json");
        for path in [&shared, &separate] {
            std::fs::write(path, serde_json::to_vec(&credential).unwrap()).unwrap();
        }
        for (name, path) in [("astra", &shared), ("terra", &shared), ("luna", &separate)] {
            let config: ModelConfig = serde_json::from_value(serde_json::json!({"kind":"chatgptSubscription", "name":name, "model":"fixture", "timeoutMs":2000, "authFile":path})).unwrap();
            factory
                .build(
                    &config,
                    runtime.handle().clone(),
                    tokio::sync::watch::channel(false).1,
                )
                .unwrap();
        }
        assert_eq!(factory.clients.lock().len(), 3);
        assert_eq!(
            factory.chatgpt_credentials.lock().len(),
            2,
            "one credential per auth file, not per model"
        );
    }
}
