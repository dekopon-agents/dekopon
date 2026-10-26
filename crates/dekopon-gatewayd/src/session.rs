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
        CancellationProbe, ConversationTurn, Handoff, History, PromptError, ReplyDisposition, Seal,
        SessionInputs, Steer, SteerSource, run_prompt_session,
    },
};
use dekopon_broker_protocol::{
    Attestation, ChatScopeClaim, ClientError, ConversationKind, DeliveredAnswer,
    DeliveredTurnRequest, DeliveryIdentity, ERROR_STORAGE_BUSY, ERROR_STORAGE_CORRUPT,
    ERROR_STORAGE_IO, ERROR_STORAGE_QUOTA, ERROR_STORAGE_TIMEOUT, ERROR_UNAUTHENTICATED,
    InvocationOutcome, InvocationResult, SessionRefusal,
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
use dekopon_process::{CallBudget, CancelHandle, CancelSignal, Limits as ShellLimits};
use dekopon_shell::CapabilityInvoker as _;
use futures_util::StreamExt as _;
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::Instrument as _;

use crate::{
    asset::{self, AssetAccess, AssetStore, RecalledAsset, SessionAssets},
    collection::Dispositions,
    config::{
        MemoryPolicy, MemoryScope, MemoryWindow, ModelConfig, RecallSource, ResolvedBroker,
        SeenReaction, Steering,
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

pub(crate) const UNMAPPED_REPLY: &str =
    "I only talk to the Lange family. I've let my owner know you stopped by.";

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
    fn build_bound(
        &self,
        model: &Arc<ModelConfig>,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError>;

    fn build_anchored(
        &self,
        _anchor: &Arc<ModelConfig>,
        model: &ModelConfig,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        self.build_bound(&Arc::new(model.clone()), runtime, cancel)
    }

    fn retire(&self) {}
}

type BoundClients = HashMap<usize, (std::sync::Weak<ModelConfig>, Arc<ModelClient>)>;
type AnchoredClients = HashMap<
    usize,
    (
        std::sync::Weak<ModelConfig>,
        Vec<(ModelConfig, Arc<ModelClient>)>,
    ),
>;

fn blocking_model(
    model: &ModelConfig,
    client: Arc<ModelClient>,
    runtime: tokio::runtime::Handle,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<SharedModel, SessionError> {
    let timeout_ms = model
        .timeout_ms()
        .ok_or_else(|| SessionError::ProxyOnlyModel {
            model: model.name().to_owned(),
        })?;
    Ok(Arc::new(BlockingModel::new(
        client,
        runtime,
        cancel,
        Duration::from_millis(timeout_ms),
    )))
}

#[derive(Default)]
pub(crate) struct ConfiguredModels {
    bound_clients: Mutex<BoundClients>,
    anchored_clients: Mutex<AnchoredClients>,
    active_auth_files: Mutex<BTreeSet<std::path::PathBuf>>,
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
    #[cfg(test)]
    pub(crate) fn credential_owners_for_test(&self) -> usize {
        self.chatgpt_credentials.lock().len()
    }

    pub(crate) fn retire(&self) {
        self.bound_clients
            .lock()
            .retain(|_, (owner, _)| owner.strong_count() > 0);
        self.anchored_clients
            .lock()
            .retain(|_, (owner, _)| owner.strong_count() > 0);
        let active = self.active_auth_files.lock();
        self.chatgpt_credentials
            .lock()
            .retain(|path, credential| active.contains(path) || Arc::strong_count(credential) > 1);
    }

    pub(crate) fn set_active_models(&self, models: &[ModelConfig]) {
        let paths = models
            .iter()
            .filter_map(|model| match model {
                ModelConfig::ChatgptSubscription { auth_file, .. } => {
                    dekopon_model::chatgpt::resolve_auth_path(auth_file.as_deref()).ok()
                }
                ModelConfig::OpenaiCompatible { .. }
                | ModelConfig::Openrouter { .. }
                | ModelConfig::Anthropic { .. } => None,
            })
            .collect();
        *self.active_auth_files.lock() = paths;
    }

    #[cfg(test)]
    pub(crate) fn bound_client_for_test(
        &self,
        model: &Arc<ModelConfig>,
    ) -> Option<Arc<ModelClient>> {
        self.bound_clients
            .lock()
            .get(&(Arc::as_ptr(model) as usize))
            .map(|(_, client)| Arc::clone(client))
    }

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
    fn build_bound(
        &self,
        model: &Arc<ModelConfig>,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        let key = Arc::as_ptr(model) as usize;
        let cached = self
            .bound_clients
            .lock()
            .get(&key)
            .and_then(|(owner, client)| {
                owner
                    .upgrade()
                    .filter(|owner| Arc::ptr_eq(owner, model))
                    .map(|_| Arc::clone(client))
            });
        let client = match cached {
            Some(client) => client,
            None => {
                let built = self.construct(model)?;
                let mut clients = self.bound_clients.lock();
                clients.retain(|index, (owner, _)| {
                    owner.strong_count() > 0
                        && (*index != key
                            || owner
                                .upgrade()
                                .is_some_and(|owner| Arc::ptr_eq(&owner, model)))
                });
                Arc::clone(
                    &clients
                        .entry(key)
                        .or_insert_with(|| (Arc::downgrade(model), built))
                        .1,
                )
            }
        };
        blocking_model(model, client, runtime, cancel)
    }

    fn build_anchored(
        &self,
        anchor: &Arc<ModelConfig>,
        model: &ModelConfig,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        let key = Arc::as_ptr(anchor) as usize;
        let live = |owner: &std::sync::Weak<ModelConfig>| {
            owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, anchor))
        };
        let cached = self
            .anchored_clients
            .lock()
            .get(&key)
            .filter(|(owner, _)| live(owner))
            .and_then(|(_, clients)| clients.iter().find(|(config, _)| config == model))
            .map(|(_, client)| Arc::clone(client));
        let client = match cached {
            Some(client) => client,
            None => {
                let built = self.construct(model)?;
                let mut anchored = self.anchored_clients.lock();
                anchored.retain(|index, (owner, _)| {
                    owner.strong_count() > 0 && (*index != key || live(owner))
                });
                let (_, clients) = anchored
                    .entry(key)
                    .or_insert_with(|| (Arc::downgrade(anchor), Vec::new()));
                if let Some((_, client)) = clients.iter().find(|(config, _)| config == model) {
                    Arc::clone(client)
                } else {
                    clients.push((model.clone(), Arc::clone(&built)));
                    built
                }
            }
        };
        blocking_model(model, client, runtime, cancel)
    }

    fn retire(&self) {
        ConfiguredModels::retire(self);
    }
}

pub(crate) struct ModelCache {
    factory: Arc<dyn ModelFactory>,
}
impl ModelCache {
    pub(crate) fn new(factory: Arc<dyn ModelFactory>) -> Self {
        Self { factory }
    }
    pub(crate) fn client_bound(
        &self,
        model: &Arc<ModelConfig>,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        self.factory.build_bound(model, runtime, cancel)
    }

    pub(crate) fn client_anchored(
        &self,
        anchor: &Arc<ModelConfig>,
        model: &ModelConfig,
        runtime: tokio::runtime::Handle,
        cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<SharedModel, SessionError> {
        self.factory.build_anchored(anchor, model, runtime, cancel)
    }

    pub(crate) fn retire(&self) {
        self.factory.retire();
    }
}

pub(crate) const MAILBOX_CAPACITY: usize = 100;
pub(crate) const MAX_INTERRUPTS: u8 = 8;

#[derive(Clone)]
pub(crate) struct SessionGate {
    stranger_reply: Arc<Mutex<Option<tokio::time::Instant>>>,
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

pub(crate) enum Taken {
    Steers(Vec<InboundMessage>),
    Sealed(Seal),
}

pub(crate) struct StopOutcome {
    pub running: CancelOutcome,
    pub dropped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageOutcome {
    Answered,
    Declined,
    Refused,
    Failed,
    ReplyFailed,
    Cancelled,
    Sealed,
    Unauthorized,
    FollowInactive,
    Steered,
    Queued,
    Busy,
    RouteLost,
}

impl MessageOutcome {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Declined => "declined",
            Self::Refused => "refused",
            Self::Failed => "failed",
            Self::ReplyFailed => "reply-failed",
            Self::Cancelled => "cancelled",
            Self::Sealed => "sealed",
            Self::Unauthorized => "unauthorized",
            Self::FollowInactive => "follow-inactive",
            Self::Steered => "steered",
            Self::Queued => "queued",
            Self::Busy => "busy",
            Self::RouteLost => "route-lost",
        }
    }

    const fn is_error(self) -> bool {
        match self {
            Self::Failed | Self::ReplyFailed => true,
            Self::Answered
            | Self::Declined
            | Self::Refused
            | Self::Cancelled
            | Self::Sealed
            | Self::Unauthorized
            | Self::FollowInactive
            | Self::Steered
            | Self::Queued
            | Self::Busy
            | Self::RouteLost => false,
        }
    }
}

pub(crate) fn record_admission(
    message: &InboundMessage,
    outcome: &str,
    depth: usize,
    cause: Option<&str>,
) {
    if let Some(cause) = cause {
        tracing::Span::current().record("busy.cause", cause);
    }
    let deliveries = match &message.message_id {
        MessageId::Github(batch) => Some(batch.deliveries()),
        MessageId::Native(_)
        | MessageId::Follow(_)
        | MessageId::Wake { .. }
        | MessageId::Job { .. } => None,
    };
    let emit = || {
        tracing::info!(
            target: "dekopon_gatewayd::audit",
            { audit.event = "gateway.admission", outcome, busy.cause = cause,
              transport = %message.transport, conversation.id = %message.conversation.key(),
              queue.depth = depth },
            "gateway.admission"
        )
    };
    match deliveries {
        Some(deliveries) => {
            dekopon_telemetry::with_event_string_array("github.deliveries", &deliveries, emit)
        }
        None => emit(),
    }
}

impl SessionGate {
    fn stranger_reply(&self) -> bool {
        let mut last = self.stranger_reply.lock();
        let now = tokio::time::Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(1)) {
            return false;
        }
        *last = Some(now);
        true
    }

    pub fn new(max_concurrent: usize) -> Self {
        Self {
            stranger_reply: Arc::default(),
            permits: Arc::new(Semaphore::new(max_concurrent)),
            refusals: Arc::new(Semaphore::new(max_concurrent)),
            in_flight: Arc::default(),
        }
    }

    pub(crate) fn admit(
        &self,
        route: &BoundRoute,
        mut message: InboundMessage,
        mut receipts: Dispositions,
    ) -> Admit {
        let key = (message.transport.clone(), message.conversation.key());
        let mut entries = self.in_flight.lock();
        if let Some(holder) = entries.get_mut(&key) {
            let Holder::Session(running) = holder else {
                record_admission(&message, MessageOutcome::Busy.label(), 0, Some("saturated"));
                return Admit::Saturated(message, receipts);
            };
            let depth = running.steers.len() + running.follow_ups.len();
            if let MessageId::Github(incoming) = &mut message.message_id
                && let Some(tail) = running.follow_ups.back_mut()
                && let MessageId::Github(batch) = &mut tail.message.message_id
                && batch.append(incoming)
            {
                tail.receipts.0.extend(std::mem::take(&mut receipts.0));
                record_admission(&tail.message, "queued", depth, None);
                return Admit::Queued;
            }
            if depth == MAILBOX_CAPACITY {
                record_admission(
                    &message,
                    MessageOutcome::Busy.label(),
                    depth,
                    Some("same-conversation"),
                );
                tracing::info!(
                    event = "gateway_steer_refused",
                    reason = "mailbox-full",
                    "gateway.steer.refused"
                );
                return Admit::Full(message, receipts);
            }
            if message.subject == running.subject
                && match message.message_id {
                    MessageId::Native(_) | MessageId::Job { .. } => true,
                    MessageId::Github(_) | MessageId::Wake { .. } | MessageId::Follow(_) => false,
                }
                && running.cancellation.state.load(Ordering::Acquire) == SESSION_RUNNING
            {
                let interrupts = match message.message_id {
                    MessageId::Native(_) | MessageId::Wake { .. } => true,
                    MessageId::Github(_) | MessageId::Job { .. } | MessageId::Follow(_) => false,
                };
                record_admission(&message, MessageOutcome::Steered.label(), depth + 1, None);
                running.steers.push_back(message);
                if running.steering == Steering::Abort
                    && interrupts
                    && running.aborts < MAX_INTERRUPTS
                {
                    running.aborts += 1;
                    running.cancellation.interrupt_model();
                }
                return Admit::Steered(receipts);
            }
            record_admission(&message, MessageOutcome::Queued.label(), depth + 1, None);
            running.follow_ups.push_back(FollowUp {
                route: route.clone(),
                message,
                receipts,
            });
            return Admit::Queued;
        }
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            record_admission(&message, MessageOutcome::Busy.label(), 0, Some("saturated"));
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

    pub(crate) fn seal_or_take(
        &self,
        key: &AdmissionKey,
        cancellation: &SessionCancellation,
    ) -> Taken {
        let mut entries = self.in_flight.lock();
        if let Some(Holder::Session(running)) = entries.get_mut(key)
            && Arc::ptr_eq(&running.cancellation.state, &cancellation.state)
            && running.cancellation.state.load(Ordering::Acquire) == SESSION_RUNNING
            && !running.steers.is_empty()
        {
            running.cancellation.rearm_model();
            return Taken::Steers(running.steers.drain(..).collect());
        }
        Taken::Sealed(seal_locked(&entries, key, cancellation))
    }

    pub(crate) fn seal(&self, key: &AdmissionKey, cancellation: &SessionCancellation) -> Seal {
        seal_locked(&self.in_flight.lock(), key, cancellation)
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

// Admission reads the running state under the same lock, so a steer is either in `steers` before
// the seal or queued as a follow-up after it.
fn seal_locked(
    entries: &BTreeMap<AdmissionKey, Holder>,
    key: &AdmissionKey,
    cancellation: &SessionCancellation,
) -> Seal {
    if !cancellation.complete() {
        return Seal::Stopped;
    }
    let leftover = match entries.get(key) {
        Some(Holder::Session(running))
            if Arc::ptr_eq(&running.cancellation.state, &cancellation.state) =>
        {
            running.steers.len()
        }
        Some(Holder::Session(_) | Holder::Probe) | None => 0,
    };
    Seal::Sealed { leftover }
}

impl SessionGate {
    pub fn refusal(&self) -> Option<OwnedSemaphorePermit> {
        let permit = Arc::clone(&self.refusals).try_acquire_owned().ok();
        if permit.is_none() {
            tracing::info!(
                event = "gateway_refusal_reply_skipped",
                reason = "refusals-full",
                "gateway.refusal.reply.skipped"
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

struct ResolvedFollowUp<T> {
    admission: SessionAdmission,
    followup: FollowUp,
    evidence: T,
}

impl SessionAdmission {
    pub(crate) fn cancellation(&self) -> SessionCancellation {
        self.cancellation.clone()
    }

    #[cfg(test)]
    pub(crate) fn next_or_release(self) -> Option<(Self, FollowUp)> {
        self.next_or_release_resolving(|_, queued| Some(queued.clone()))
    }

    #[cfg(test)]
    pub(crate) fn next_or_release_resolving(
        self,
        resolve: impl Fn(&InboundMessage, &BoundRoute) -> Option<BoundRoute>,
    ) -> Option<(Self, FollowUp)> {
        self.next_or_release_with(&|message, queued| {
            resolve(message, queued).map(|route| (route, ()))
        })
        .map(|resolved| (resolved.admission, resolved.followup))
    }

    fn next_or_release_with<T>(
        mut self,
        resolve: &impl Fn(&InboundMessage, &BoundRoute) -> Option<(BoundRoute, T)>,
    ) -> Option<ResolvedFollowUp<T>> {
        let next = {
            let mut entries = self.in_flight.lock();
            let Some(Holder::Session(running)) = entries.get_mut(&self.key) else {
                return None;
            };
            // Fold before changing subject: leftover steers must never drain into another sender's run.
            let mut leftovers = Vec::new();
            while let Some(mut message) = running.steers.pop_front() {
                let folds = match message.message_id {
                    MessageId::Native(_) | MessageId::Wake { .. } => true,
                    MessageId::Github(_) | MessageId::Job { .. } | MessageId::Follow(_) => false,
                };
                if folds {
                    let mut grouped = Vec::new();
                    while let Some(next) = running.steers.pop_front_if(|steer| folds_in(steer)) {
                        grouped.push(next);
                    }
                    fold(&mut message, grouped);
                }
                leftovers.push(FollowUp {
                    route: (*self.route).clone(),
                    receipts: crate::collection::Dispositions(message.constituents.clone()),
                    message,
                });
            }
            for leftover in leftovers.into_iter().rev() {
                running.follow_ups.push_front(leftover);
            }
            let mut found = None;
            while let Some(mut next) = running.follow_ups.pop_front() {
                let Some((route, evidence)) = resolve(&next.message, &next.route) else {
                    next.receipts.finish(MessageOutcome::RouteLost);
                    continue;
                };
                if folds_in(&next.message) {
                    let mut grouped = Vec::new();
                    while let Some(mut same) = running.follow_ups.pop_front_if(|queued| {
                        folds_in(&queued.message) && queued.message.subject == next.message.subject
                    }) {
                        next.receipts.0.append(&mut same.receipts.0);
                        grouped.push(same.message);
                    }
                    if !grouped.is_empty() {
                        fold(&mut next.message, grouped);
                    }
                }
                next.route = route;
                running.subject = next.message.subject.clone();
                running.cancellation = SessionCancellation::new();
                running.steering = next.route.steering;
                running.aborts = 0;
                running.received_at = next.message.received_at;
                self.cancellation = running.cancellation.clone();
                *self.route = next.route.clone();
                found = Some((next, evidence));
                break;
            }
            if found.is_none() {
                entries.remove(&self.key);
            }
            found
        };
        next.map(|(followup, evidence)| ResolvedFollowUp {
            admission: self,
            followup,
            evidence,
        })
    }
}

fn folds_in(message: &InboundMessage) -> bool {
    match message.message_id {
        MessageId::Native(_) => true,
        MessageId::Wake { .. }
        | MessageId::Github(_)
        | MessageId::Job { .. }
        | MessageId::Follow(_) => false,
    }
}

fn fold(lead: &mut InboundMessage, grouped: Vec<InboundMessage>) {
    lead.text = bound_inbound(&crate::collection::combined_text(
        std::iter::once(&*lead).chain(grouped.iter()),
    ));
    for next in grouped {
        if let MessageId::Native(id) = next.message_id {
            lead.folded.push(id);
        }
        lead.folded.extend(next.folded);
        lead.assets.extend(next.assets);
        lead.constituents.extend(next.constituents);
        lead.receive_span = next.receive_span;
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

    #[must_use]
    fn complete(&self) -> bool {
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
                self.handle.cancel();
            }
            won
        };
        if cancelled {
            self.model.send_replace(true);
        }
        cancelled
    }

    pub(crate) fn source(&self) -> Option<CancelSource> {
        *self.source.lock()
    }

    pub(crate) async fn cancelled(&self) {
        self.signal.cancelled().await;
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
    pub follow: Option<Arc<crate::follow::FollowTable>>,
    pub assets: Arc<AssetStore>,
    pub asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>>,
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
                            delta = 0,
                            "gateway.recalled"
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
                            reason = error.label(),
                            "gateway.recall.failed"
                        );
                        None
                    }
                    Err(_) => {
                        tracing::warn!(
                            event = "gateway_recall_failed",
                            source = "journal",
                            reason = "task",
                            "gateway.recall.failed"
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
                    delta = 0,
                    "gateway.recalled"
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
            delta = messages.len(),
            "gateway.recalled"
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
            MessageId::Native(id) | MessageId::Follow(id) => Some(id.as_str()),
            MessageId::Github(_) | MessageId::Wake { .. } | MessageId::Job { .. } => None,
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
        tracing::warn!(
            event = "gateway_recall_failed",
            source = "platform",
            reason,
            "gateway.recall.failed"
        );
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
    RecalledWindow {
        history: History::from_turns(window.limits, turns),
        assets,
        next_asset_id,
        newest,
    }
}

impl Delta {
    fn block(self, store: &AssetStore, access: &AssetAccess) -> Option<String> {
        if self.messages.is_empty() {
            return None;
        }
        let arriving = self
            .messages
            .iter()
            .flat_map(|message| message.assets.iter().cloned())
            .collect();
        let registered = store.assets_for_access(access, arriving, Instant::now());
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

fn conversation_key(route: &BoundRoute, message: &InboundMessage) -> ConversationKey {
    let subject = match route.memory {
        MemoryPolicy::Persistent(MemoryWindow {
            scope: MemoryScope::SharedConversation,
            ..
        }) => None,
        MemoryPolicy::OneShot
        | MemoryPolicy::Persistent(MemoryWindow {
            scope: MemoryScope::PrivateConversation,
            ..
        }) => Some(&message.subject),
    };
    ConversationKey::of(
        &route.agent,
        &route.transport,
        &message.conversation,
        subject,
    )
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
fn attribution_header(principal: Option<&PrincipalId>) -> String {
    match principal {
        Some(principal) => format!("[gateway: authenticated participant: {principal}]\n"),
        None => "[gateway: unmapped participant]\n".to_owned(),
    }
}

fn attributed_prompt(principal: Option<&PrincipalId>, text: &str) -> String {
    bound_inbound(&format!("{}{text}", attribution_header(principal)))
}

#[derive(Clone, Copy)]
enum InventoryPath {
    Turn,
    Follow,
    Steer,
}

struct PromptFrame<'a> {
    inventory: Option<&'a Mutex<asset::InventoryUsage>>,
    principal: Option<&'a PrincipalId>,
    delivery: Option<&'a str>,
    stop: Option<&'a str>,
}

impl PromptFrame<'_> {
    fn inventory_prompt(
        &self,
        text: &str,
        registered: &asset::Registered,
        images_supported: bool,
        reader: asset::TextReader,
        notice_bytes: usize,
    ) -> String {
        let original_text = text;
        let text = if text.trim().is_empty() { "" } else { text };
        let separator = if text.is_empty() { "" } else { "\n\n" };
        let budget = crate::transport::MAX_INBOUND_TEXT_BYTES
            .saturating_sub(text.len())
            .saturating_sub(separator.len())
            .saturating_sub(attribution_header(self.principal).len())
            .saturating_sub(notice_bytes);
        let note = asset::reference_note(registered, images_supported, reader, budget);
        if let Some(inventory) = self.inventory {
            inventory.lock().note(&note, registered);
        }
        debug_assert_eq!(note.listed + note.omitted, registered.inventory.len());
        debug_assert!(note.bytes <= budget);
        if note.text.is_empty() {
            original_text.to_owned()
        } else {
            format!("{text}{separator}{}", note.text)
        }
    }

    fn inventory(
        &self,
        text: &str,
        registered: &asset::Registered,
        images_supported: bool,
        reader: asset::TextReader,
        path: InventoryPath,
    ) -> String {
        let reserved = self.delivery.map_or(0, |notice| notice.len() + 1)
            + self.stop.map_or(0, |notice| notice.len() + 1);
        let text = self.inventory_prompt(text, registered, images_supported, reader, reserved);
        let text = match path {
            InventoryPath::Turn | InventoryPath::Follow => match self.stop {
                Some(notice) => bound_inbound(&format!("{notice}\n{text}")),
                None => text,
            },
            InventoryPath::Steer => text,
        };
        let text = match path {
            InventoryPath::Turn => match self.delivery {
                Some(notice) => bound_inbound(&format!("{notice}\n{text}")),
                None => text,
            },
            InventoryPath::Follow | InventoryPath::Steer => text,
        };
        attributed_prompt(self.principal, &text)
    }
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
        otel.status_code = tracing::field::Empty,
        busy.cause = tracing::field::Empty,
        batch.members = receipts.0.len(),
    );
    for receipt in &receipts.0 {
        dekopon_telemetry::link_span(&span, receipt);
    }
    span
}

#[cfg(test)]
pub(crate) fn run_session(
    runner: Arc<SessionRunner>,
    route: BoundRoute,
    message: InboundMessage,
    driver: Arc<dyn ChatDriver>,
) -> impl std::future::Future<Output = ()> + Send {
    run_session_resolving(
        runner,
        route,
        message,
        driver,
        (String::new(), None),
        |_, queued| Some((queued.clone(), String::new())),
    )
}

pub(crate) fn run_session_resolving(
    runner: Arc<SessionRunner>,
    route: BoundRoute,
    message: InboundMessage,
    driver: Arc<dyn ChatDriver>,
    (digest, ticket): (String, Option<crate::transport::github::AdmissionTicket>),
    resolve: impl Fn(&InboundMessage, &BoundRoute) -> Option<(BoundRoute, String)> + Send + 'static,
) -> impl std::future::Future<Output = ()> + Send {
    let receipts = match &message.message_id {
        MessageId::Github(_) => Dispositions(vec![message.receive_span.clone()]),
        MessageId::Native(_)
        | MessageId::Follow(_)
        | MessageId::Wake { .. }
        | MessageId::Job { .. } => Dispositions(message.constituents.clone()),
    };
    async move {
        let span = message_span(&route, &message, &receipts);
        let mut held_by = dekopon_telemetry::span_trace_context(&span);
        let mut admission = execute(
            &runner,
            &route,
            message,
            &driver,
            receipts,
            (&digest, ticket),
        )
        .instrument(span)
        .await;
        while let Some(resolved) =
            admission.and_then(|admission| admission.next_or_release_with(&resolve))
        {
            let ResolvedFollowUp {
                admission: next,
                followup,
                evidence: digest,
            } = resolved;
            let span = message_span(&followup.route, &followup.message, &followup.receipts);
            if let Some(parts) = held_by {
                dekopon_telemetry::link_remote(&span, parts);
            }
            held_by = dekopon_telemetry::span_trace_context(&span);
            admission = Some(
                run_admitted(
                    &runner,
                    &followup.route,
                    followup.message,
                    &driver,
                    followup.receipts,
                    (next, &digest),
                )
                .instrument(span)
                .await,
            );
        }
        drop(route);
        runner.models.retire();
    }
}

async fn execute(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    receipts: Dispositions,
    (digest, ticket): (&str, Option<crate::transport::github::AdmissionTicket>),
) -> Option<SessionAdmission> {
    if message.constituents.is_empty() {
        crate::collection::record_received(&message);
    }
    let liveness = message.liveness.clone();
    let message_kind = message.conversation.kind;
    let is_unattended = match message.message_id {
        MessageId::Native(_) => false,
        MessageId::Github(_)
        | MessageId::Wake { .. }
        | MessageId::Job { .. }
        | MessageId::Follow(_) => true,
    };
    let notice = crate::jobs::Notice::of(&message);
    let admitted = runner.gate.admit(route, message, receipts);
    if let Some(ticket) = ticket {
        use crate::transport::github::Acked;
        let answer = match &admitted {
            Admit::Admitted(..) => Acked::Admitted,
            Admit::Queued => Acked::Queued,
            Admit::Steered(_) | Admit::Full(..) | Admit::Saturated(..) => Acked::Busy,
        };
        ticket.answer(answer);
    }
    match admitted {
        Admit::Admitted(admission, message, receipts) => {
            if let Some(notice) = &notice {
                notice.record("new-turn");
            }
            Some(
                run_admitted(
                    runner,
                    route,
                    message,
                    driver,
                    receipts,
                    (admission, digest),
                )
                .await,
            )
        }
        Admit::Steered(receipts) => {
            if let Some(notice) = &notice {
                notice.record("steered");
            }
            tracing::Span::current().record("outcome", MessageOutcome::Steered.label());
            receipts.finish(MessageOutcome::Steered);
            acknowledge_steer(driver.as_ref(), liveness.as_ref(), route, message_kind).await;
            None
        }
        Admit::Queued => {
            if let Some(notice) = &notice {
                notice.record("queued");
            }
            tracing::Span::current().record("outcome", MessageOutcome::Queued.label());
            if !is_unattended {
                acknowledge_steer(driver.as_ref(), liveness.as_ref(), route, message_kind).await;
            }
            None
        }
        Admit::Full(message, receipts) | Admit::Saturated(message, receipts) => {
            if let Some(notice) = &notice {
                notice.record("dropped");
            }
            tracing::info!(
                event = "gateway_session_rejected",
                reason = "busy",
                "gateway.session.rejected"
            );
            let can_reply = match &message.message_id {
                MessageId::Github(_) | MessageId::Follow(_) => false,
                MessageId::Native(_) | MessageId::Wake { .. } | MessageId::Job { .. } => true,
            };
            if can_reply
                && notice.is_none()
                && (runner.reply_on_busy || !message.constituents.is_empty())
                && let Some(_reply) = runner.gate.refusal()
            {
                answer(driver, &message, BUSY_REPLY).await;
            }
            tracing::Span::current().record("outcome", MessageOutcome::Busy.label());
            receipts.finish(MessageOutcome::Busy);
            None
        }
    }
}

fn permission_reply(principal: &PrincipalId) -> String {
    format!("Sorry {principal}, you don't have permission for this agent.")
}

async fn acknowledge_steer(
    driver: &dyn ChatDriver,
    target: Option<&crate::transport::LivenessTarget>,
    route: &BoundRoute,
    kind: dekopon_broker_protocol::ConversationKind,
) {
    if route.indicators.get(&kind).is_some_and(|resolved| {
        resolved.settings.mode == crate::config::LivenessMode::Native
            && resolved.settings.seen == SeenReaction::On
    }) && let (Some(ack), Some(target)) = (driver.steer_ack(), target)
        && let Err(error) = crate::progress::bounded(ack.seen(target)).await
    {
        tracing::debug!(event = "gateway_steer_ack_failed", transport = %route.transport, error = %error, "gateway.steer.ack.failed");
    }
}

async fn run_admitted(
    runner: &SessionRunner,
    route: &BoundRoute,
    mut message: InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    receipts: Dispositions,
    (admission, digest): (SessionAdmission, &str),
) -> SessionAdmission {
    if let MessageId::Github(batch) = &message.message_id {
        message.text = batch.note(&message.subject);
        record_admission(&message, "started", 0, None);
    }
    let cancellation = admission.cancellation();
    let Ended { outcome, journal } = session(runner, route, &message, driver, cancellation.clone())
        .instrument(tracing::info_span!(
            "gateway.session",
            generation.digest = digest,
            agent = %route.agent,
            gen_ai.agent.name = %route.agent,
            gen_ai.operation.name = "invoke_agent",
            conversation.kind = message.conversation.kind.as_str(),
            conversation.container = message.conversation.container.as_deref().unwrap_or_default(),
            conversation.id = message.conversation.id.as_str(),
            conversation.thread = message.conversation.thread.as_deref().unwrap_or_default(),
            conversation.turns = tracing::field::Empty,
            conversation.bytes = tracing::field::Empty,
            conversation.arena_bytes = tracing::field::Empty,
            conversation.recall_source = tracing::field::Empty,
            conversation.recalled_turns = tracing::field::Empty,
            conversation.carried_assets = tracing::field::Empty,
        ))
        .await;
    let span = tracing::Span::current();
    span.record("outcome", outcome.label());
    if outcome.is_error() {
        span.record("otel.status_code", "error");
    }
    receipts.finish(outcome);
    if let Some(journal) = journal {
        crate::memory::generation::persist(runner, route, &cancellation, journal).await;
    }
    admission
}

struct Ended {
    outcome: MessageOutcome,
    journal: Option<crate::memory::generation::PostReply>,
}

impl From<MessageOutcome> for Ended {
    fn from(outcome: MessageOutcome) -> Self {
        Self {
            outcome,
            journal: None,
        }
    }
}

struct SessionSteers {
    gate: SessionGate,
    key: AdmissionKey,
    cancellation: SessionCancellation,
    store: Arc<AssetStore>,
    access: AssetAccess,
    images_supported: bool,
    text_reader: asset::TextReader,
    delivery_notice: Option<&'static str>,
    stop_notice: Option<String>,
    principal: Option<PrincipalId>,
    received_at: tokio::time::Instant,
    inventory: Arc<Mutex<asset::InventoryUsage>>,
    raw_texts: Mutex<Vec<String>>,
    taken: Mutex<Vec<String>>,
}

impl SteerSource for SessionSteers {
    fn take(&self) -> Vec<Steer> {
        self.stage(self.gate.take_steers(&self.key))
    }

    fn seal_or_take(&self) -> Handoff {
        match self.gate.seal_or_take(&self.key, &self.cancellation) {
            Taken::Steers(steers) => Handoff::Steers(self.stage(steers)),
            Taken::Sealed(seal) => Handoff::Sealed(seal),
        }
    }

    fn seal(&self) -> Seal {
        self.gate.seal(&self.key, &self.cancellation)
    }
}

impl SessionSteers {
    fn stage(&self, steers: Vec<InboundMessage>) -> Vec<Steer> {
        steers
            .into_iter()
            .map(|mut steer| {
                match &steer.message_id {
                    MessageId::Job { .. } => return Steer::Notice(steer.text),
                    MessageId::Native(id) => self.taken.lock().push(id.clone()),
                    MessageId::Github(_) | MessageId::Wake { .. } | MessageId::Follow(_) => {}
                }
                self.taken.lock().append(&mut steer.folded);
                let seconds = steer
                    .received_at
                    .saturating_duration_since(self.received_at)
                    .as_secs();
                let text = bound_inbound(&format!(
                    "[gateway: sent while you were working, +{seconds}s]\n{}",
                    steer.text
                ));
                let registered =
                    self.store
                        .assets_for_access(&self.access, steer.assets, Instant::now());
                let recorded =
                    bound_inbound(&format!("{text}{}", asset::arrival_markers(&registered)));
                let prompt = PromptFrame {
                    inventory: Some(&self.inventory),
                    principal: self.principal.as_ref(),
                    delivery: self.delivery_notice,
                    stop: self.stop_notice.as_deref(),
                }
                .inventory(
                    &text,
                    &registered,
                    self.images_supported,
                    self.text_reader,
                    InventoryPath::Steer,
                );
                let recorded = attributed_prompt(self.principal.as_ref(), &recorded);
                self.raw_texts.lock().push(steer.text);
                Steer::Person { prompt, recorded }
            })
            .collect()
    }

    fn user_text(&self, prompt: &str) -> String {
        let mut text = prompt.to_owned();
        for steer in self.raw_texts.lock().iter() {
            text.push_str("\n\n");
            text.push_str(steer);
        }
        text
    }
}

async fn session(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: &InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    cancellation: SessionCancellation,
) -> Ended {
    runner.assets.finish_publications().await;
    let mut publication = None;
    let ended = session_turn(
        runner,
        route,
        message,
        driver,
        cancellation,
        &mut publication,
    )
    .await;
    drop(publication);
    runner.assets.finish_publications().await;
    ended
}

#[expect(
    clippy::too_many_lines,
    clippy::wildcard_enum_match_arm,
    reason = "reshaped by the unit that next rewrites this"
)]
async fn session_turn(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: &InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    cancellation: SessionCancellation,
    publication: &mut Option<asset::Publication>,
) -> Ended {
    // Only trusted route configuration decides whether the subject participates in the state key;
    // message text, transport presentation, and model output never influence that choice.
    let mut key = conversation_key(route, message);
    let is_follow = matches!(message.message_id, MessageId::Follow(_));
    if is_follow {
        let ticket = runner.follow.as_ref().map(|table| {
            let follow_key = crate::follow::follow_key(
                &route.transport,
                route.agent.as_str(),
                &route.conversation,
                &message.conversation.key(),
            );
            table.ticket(&follow_key, SystemTime::now())
        });
        if !matches!(ticket, Some(crate::follow::FollowTicket::Active { .. })) {
            tracing::info!(
                audit.event = "gateway.follow",
                operation = "refused",
                route = route.route_index,
                reason = "inactive",
                "gateway.follow"
            );
            return MessageOutcome::FollowInactive.into();
        }
    }
    let mut sealed = sealed_conversation(
        runner,
        &key,
        route
            .memory
            .window()
            .map_or(crate::config::DEFAULT_FORGET_AFTER, |window| {
                window.forget_after
            }),
    )
    .await;
    if reject_sealed_thread(runner, message, driver, sealed).await {
        return MessageOutcome::Sealed.into();
    }
    let leg = match connect(runner, route, message).await {
        Ok(leg) => leg,
        Err(SessionError::BrokerLeg(BrokerLegError::Client(ClientError::SessionRefused(
            refusal,
        )))) => {
            revoke_thread_ownership(runner, message);
            let reply = match refusal {
                SessionRefusal::UnmappedSubject => {
                    tracing::info!(
                        audit.event = "gateway.session.rejected",
                        event = "gateway_session_rejected",
                        transport = message.transport.as_str(),
                        route = route.route_index,
                        conversation.kind = message.conversation.kind.as_str(),
                        conversation.container = message
                            .conversation
                            .container
                            .as_deref()
                            .unwrap_or_default(),
                        conversation.id = message.conversation.id.as_str(),
                        conversation.thread =
                            message.conversation.thread.as_deref().unwrap_or_default(),
                        subject = message.subject.canonical(),
                        reason = "unmapped-subject",
                        "gateway.session.rejected"
                    );
                    if !runner.gate.stranger_reply() {
                        return MessageOutcome::Unauthorized.into();
                    }
                    UNMAPPED_REPLY.to_owned()
                }
                SessionRefusal::AgentDenied { principal } => {
                    tracing::info!(
                        event = "gateway_session_rejected",
                        transport = message.transport.as_str(),
                        route = route.route_index,
                        conversation.kind = message.conversation.kind.as_str(),
                        conversation.container = message
                            .conversation
                            .container
                            .as_deref()
                            .unwrap_or_default(),
                        conversation.id = message.conversation.id.as_str(),
                        conversation.thread =
                            message.conversation.thread.as_deref().unwrap_or_default(),
                        subject = message.subject.canonical(),
                        reason = "agent-denied",
                        "gateway.session.rejected"
                    );
                    permission_reply(&principal)
                }
                SessionRefusal::Other => {
                    tracing::info!(
                        event = "gateway_session_rejected",
                        reason = "attestation-refused",
                        "gateway.session.rejected"
                    );
                    UNAUTHORIZED_REPLY.to_owned()
                }
            };
            if !is_follow && answer(driver, message, &reply).await {
                reply_delivered(MessageOutcome::Unauthorized);
            }
            return MessageOutcome::Unauthorized.into();
        }
        Err(SessionError::BrokerLeg(BrokerLegError::Client(ClientError::Remote {
            code, ..
        }))) if code == ERROR_UNAUTHENTICATED => {
            revoke_thread_ownership(runner, message);
            tracing::info!(
                event = "gateway_session_rejected",
                reason = "attestation-refused",
                "gateway.session.rejected"
            );
            if !is_follow && answer(driver, message, UNAUTHORIZED_REPLY).await {
                reply_delivered(MessageOutcome::Unauthorized);
            }
            return MessageOutcome::Unauthorized.into();
        }
        Err(error) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category(),
                error = %error,
                "gateway.session.failed"
            );
            if !is_follow && answer(driver, message, FAILURE_REPLY).await {
                reply_delivered(MessageOutcome::Failed);
            }
            return MessageOutcome::Failed.into();
        }
    };
    let space_grants = match route.memory {
        MemoryPolicy::Persistent(MemoryWindow {
            scope: MemoryScope::SharedConversation,
            ..
        }) => leg.space_grants(),
        MemoryPolicy::OneShot
        | MemoryPolicy::Persistent(MemoryWindow {
            scope: MemoryScope::PrivateConversation,
            ..
        }) => None,
    };
    if space_grants.is_some() {
        key = ConversationKey::for_space(
            dekopon_core::SpaceKey {
                agent: route.agent.clone(),
                transport: message.transport.clone().into(),
                workspace: message.conversation.container.clone().map(Into::into),
                channel: message.conversation.id.clone().into(),
            },
            message.conversation.thread.clone(),
        );
        sealed = sealed_conversation(
            runner,
            &key,
            route
                .memory
                .window()
                .map_or(crate::config::DEFAULT_FORGET_AFTER, |window| {
                    window.forget_after
                }),
        )
        .await;
        if reject_sealed_thread(runner, message, driver, sealed).await {
            return MessageOutcome::Sealed.into();
        }
    }
    // Same-sender steers use this leg; other senders and wakes open their own follow-up leg.
    let granted = leg.granted();
    // Removing the entry, not just refusing further calls, matters because a revoked subject's
    // exchange left resident for its idle timeout would hold exactly the text the revocation was
    // about.
    if granted.is_empty() {
        revoke_thread_ownership(runner, message);
        if space_grants.is_none() {
            runner
                .conversations
                .remove(&key, EvictionReason::GrantChanged);
        }
        tracing::info!(
            event = "gateway_session_rejected",
            transport = message.transport.as_str(),
            route = route.route_index,
            conversation.kind = message.conversation.kind.as_str(),
            conversation.container = message
                .conversation
                .container
                .as_deref()
                .unwrap_or_default(),
            conversation.id = message.conversation.id.as_str(),
            conversation.thread = message.conversation.thread.as_deref().unwrap_or_default(),
            subject = message.subject.canonical(),
            reason = "unauthorized",
            "gateway.session.rejected"
        );
        let reply = leg
            .principal()
            .map_or_else(|| UNAUTHORIZED_REPLY.to_owned(), permission_reply);
        if !is_follow && answer(driver, message, &reply).await {
            reply_delivered(MessageOutcome::Unauthorized);
        }
        return MessageOutcome::Unauthorized.into();
    }
    let granted = space_grants.map_or(granted, |grants| {
        grants.iter().map(ToString::to_string).collect()
    });
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
        MessageId::Native(id) | MessageId::Follow(id) => Some(id.clone()),
        MessageId::Github(_) | MessageId::Wake { .. } | MessageId::Job { .. } => None,
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
                let store = Arc::clone(&runner.assets);
                let access = assets.clone();
                let subscriber = tracing::dispatcher::get_default(Clone::clone);
                let restore_span = span.clone();
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    tracing::dispatcher::with_default(&subscriber, || {
                        let _entered = restore_span.enter();
                        store.restore(&access, recalled_assets, next_asset_id, Instant::now());
                    });
                })
                .await
                {
                    tracing::error!(event = "gateway_session_failed", category = "asset-task", %error, "gateway.session.failed");
                    return MessageOutcome::Failed.into();
                }
            }
            (history, cache_key, Some(lease), assets, delta)
        }
        None => (
            History::default(),
            runner.conversations.route_cache_key(route),
            None,
            AssetAccess::one_shot(key.clone()),
            None,
        ),
    };
    let span = tracing::Span::current();
    span.record("conversation.turns", seeded.len());
    span.record("conversation.bytes", seeded.bytes());
    span.record(
        "conversation.arena_bytes",
        runner
            .conversations
            .snapshot(&key, &granted)
            .map_or(0, |arena| arena.bytes()),
    );
    tracing::info!(
        target: "dekopon_gatewayd::audit",
        {
            audit.event = "gateway.session.cache_key",
            prompt.cache_key = cache_key.as_str(),
            conversation.persistent = window.is_some(),
        },
        "gateway.session.cache_key"
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
    *publication = runner.assets.publication(&asset_access);
    let store = Arc::clone(&runner.assets);
    let access = asset_access.clone();
    let arriving = message.assets.clone();
    let asset_subscriber = tracing::dispatcher::get_default(Clone::clone);
    let asset_span = tracing::Span::current();
    let (delta, registered) = match tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&asset_subscriber, || {
            let _entered = asset_span.enter();
            let delta = delta
                .and_then(|delta| delta.block(&store, &access))
                .map(ConversationTurn::unanswered);
            let registered = store.assets_for_access(&access, arriving, Instant::now());
            (delta, registered)
        })
    })
    .await
    {
        Ok(registered) => registered,
        Err(error) => {
            tracing::error!(event = "gateway_session_failed", category = "asset-task", %error, "gateway.session.failed");
            return MessageOutcome::Failed.into();
        }
    };
    if let Some(delta) = &delta {
        seeded.record(delta.clone());
    }
    let text_reader = if leg.is_granted("asset.cat") {
        asset::TextReader::AssetCat
    } else {
        asset::TextReader::Absent
    };
    let recorded = bound_inbound(&format!(
        "{}{}",
        message.text,
        asset::arrival_markers(&registered)
    ));
    let journal_access = asset_access.clone();
    let delivery_notice = runner.assets.take_delivery_notice(&asset_access);
    let stop_notice = runner
        .pending_notices
        .lock()
        .remove(&key)
        .map(|PendingNotice(cause)| cause.notice());
    let inventory = Arc::new(Mutex::new(asset::InventoryUsage::default()));
    let text = PromptFrame {
        inventory: Some(&inventory),
        principal: principal.as_ref(),
        delivery: delivery_notice,
        stop: stop_notice.as_deref(),
    }
    .inventory(
        &message.text,
        &registered,
        images_supported,
        text_reader,
        if is_follow {
            InventoryPath::Follow
        } else {
            InventoryPath::Turn
        },
    );
    let frame = |text: String| {
        let text = match &delivery_notice {
            Some(note) => bound_inbound(&format!("{note}\n{text}")),
            None => text,
        };
        attributed_prompt(principal.as_ref(), &text)
    };
    let recorded = if is_follow {
        attributed_prompt(principal.as_ref(), &recorded)
    } else {
        frame(recorded)
    };
    let mut assets = SessionAssets::new(
        Arc::clone(&runner.assets),
        asset_access.clone(),
        runner.asset_fetchers.get(&message.transport).cloned(),
        tokio::runtime::Handle::current(),
        images_supported,
    );
    assets.inventory = Arc::clone(&inventory);
    let assets = Arc::new(assets);
    let steers = Arc::new(SessionSteers {
        inventory,
        gate: runner.gate.clone(),
        key: (message.transport.clone(), message.conversation.key()),
        cancellation: cancellation.clone(),
        store: Arc::clone(&runner.assets),
        access: asset_access,
        images_supported,
        text_reader,
        delivery_notice,
        stop_notice,
        principal: principal.clone(),
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

    let liveness = route
        .indicators
        .get(&message.conversation.kind)
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
    let (mut settings, keep_alive) = (liveness.settings, liveness.keep_alive.clone());
    if is_follow {
        settings.mode = crate::config::LivenessMode::Off;
    }
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
        silent_until_success: is_follow,
    });
    sink.emit(ProgressEvent::Started {
        agent: route.agent.to_string(),
        max_steps: limits.max_steps,
    });
    // The prompt loop and interpreter are synchronous and can block for a long time; running that
    // on a runtime worker would stall every other session in the process.
    let blocking_span = span.clone();
    let prompt_cancellation = cancellation.clone();
    let reply_optional = is_follow
        || message
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
    let follow_ticket = route
        .follow
        .then(|| {
            runner.follow.as_ref().map(|table| {
                (
                    Arc::clone(table),
                    crate::follow::follow_key(
                        &route.transport,
                        route.agent.as_str(),
                        &route.conversation,
                        &message.conversation.key(),
                    ),
                    route.route_index,
                )
            })
        })
        .flatten();
    let invoker = crate::follow::FollowInvoker::new(
        leg,
        if matches!(
            message.message_id,
            MessageId::Native(_) | MessageId::Follow(_)
        ) {
            follow_ticket
        } else {
            None
        },
    );
    let leave_request = invoker.leave_request();
    let capture_limits = route.memory_policy.as_ref().and_then(|policy| {
        (runner.journal.is_some() && matches!(route.memory, MemoryPolicy::Persistent(_))).then(
            || {
                let bound = |limit| match limit {
                    crate::config::memory::CaptureBound::Bytes(bytes) => Some(bytes.get() as usize),
                    crate::config::memory::CaptureBound::Word(_) => None,
                };
                (
                    bound(policy.capture.tool_request),
                    bound(policy.capture.tool_result),
                )
            },
        )
    });
    let memory_journal = runner
        .journal
        .as_ref()
        .filter(|_| matches!(route.memory, MemoryPolicy::Persistent(_)))
        .cloned();
    let memory_stem = key.journal_stem();
    let memory_grant = journal::grant_digest(&granted);
    let selection_policy = route.memory_policy.clone().unwrap_or_default();
    let selection_seal = sealed.map(|seal| seal.at);
    let selection_window = window.filter(|window| window.recall == RecallSource::Journal);
    let model_runtime = tokio::runtime::Handle::current();
    let model_cancel = cancellation.model_watch();
    let subscriber = tracing::dispatcher::get_default(Clone::clone);
    let result = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&subscriber, || {
            let _entered = blocking_span.enter();
            let model = match models.client_bound(&model_config, model_runtime, model_cancel) {
                Ok(model) => ChatProviderGovernor::wrap(model, &metering, &agent_id, &model_config),
                Err(error) => return (Err(error), None, Vec::new()),
            };
            let runtime = ShellRuntime {
                invoker,
                limits: shell,
                calls,
            };
            let mut history = seeded;
            if let (Some(journal), Some(window)) = (memory_journal.as_ref(), selection_window) {
                let policy = &selection_policy;
                match journal.project(
                    journal::Projection {
                        stem: &memory_stem,
                        grant: &memory_grant,
                        window,
                        sealed_at: selection_seal,
                        policy,
                    },
                    &history,
                    |available, evidence| {
                        journal::decide(&runtime, &policy.selection, available, evidence)
                    },
                ) {
                    Ok(projected) => history = projected,
                    Err(error) => tracing::warn!(
                        event = "gateway_recall_failed",
                        source = "journal",
                        reason = error.label(),
                        "gateway.recall.failed"
                    ),
                }
            }
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
            let memory_tools = memory_journal.as_ref().map(|journal| {
                journal::JournalMemoryTools::new(
                    journal,
                    memory_stem,
                    memory_grant,
                    &selection_policy,
                )
            });
            if let Some(tools) = memory_tools.as_ref() {
                inputs = inputs.with_memory_tools(tools);
            }
            if let Some((request, result)) = capture_limits {
                inputs = inputs.with_exchange_capture(request, result);
            }
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
                _ => history
                    .turns()
                    .last()
                    .cloned()
                    .map(|turn| turn.with_memory_id(journal::mint_turn_id())),
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

    let (outcome, turn, images) = match result.map_err(TaskFailure::from) {
        Ok(session) => session,
        Err(failure) => {
            match steers.seal() {
                Seal::Stopped => {
                    return stopped(runner, route, &key, message, &mut progress, &cancellation)
                        .await
                        .into();
                }
                Seal::Sealed { .. } => {}
            }
            progress.seal();
            tracing::error!(
                event = "gateway_session_failed",
                category = "session-task",
                failure = failure.category(),
                error = %failure,
                "gateway.session.failed"
            );
            let terminal = if is_follow {
                Terminal::Silent
            } else {
                remember_stop(runner, &key, StopCause::SessionTask)
            };
            let replied = progress.terminal(terminal).await;
            if replied {
                reply_delivered(MessageOutcome::Failed);
                return MessageOutcome::Failed.into();
            }
            return MessageOutcome::ReplyFailed.into();
        }
    };

    if matches!(&outcome, Err(SessionError::Prompt(PromptError::Cancelled)))
        || match steers.seal() {
            Seal::Stopped => true,
            Seal::Sealed { .. } => false,
        }
    {
        return stopped(runner, route, &key, message, &mut progress, &cancellation)
            .await
            .into();
    }

    progress.seal();
    let mut pending_journal = None;

    if let Some(window) = window
        && let Some(turn) = turn
        && let Some(lease) = conversation_lease
    {
        let taken = TakenIn {
            newest: newest_seen,
            steers: std::mem::take(&mut *steers.taken.lock()),
            recalled: delta,
        };
        if lease.commit(
            window,
            turn.without_exchanges(),
            taken,
            &cache_key,
            Instant::now(),
        ) && runner.journal.is_some()
        {
            pending_journal = Some(crate::memory::generation::PostReply {
                key: key.clone(),
                granted,
                access: journal_access,
                publication: publication.take().map(asset::Publication::seal),
                window,
                turn,
            });
        }
    }

    if matches!(
        &outcome,
        Ok(outcome) if outcome.disposition == ReplyDisposition::Suppress
    ) {
        progress.terminal(Terminal::Silent).await;
        return Ended {
            outcome: MessageOutcome::Declined,
            journal: pending_journal,
        };
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
            (
                Terminal::Answered(reply),
                MessageOutcome::Answered,
                Some(text),
            )
        }
        Err(SessionError::Prompt(PromptError::Model(InferenceError::OverBudget(refusal)))) => {
            tracing::info!(
                event = "gateway_session_refused",
                category = "over-budget",
                "gateway.session.refused"
            );
            (
                Terminal::Failed(bound_outbound(&refusal.to_string())),
                MessageOutcome::Refused,
                None,
            )
        }
        Err(SessionError::Prompt(PromptError::UnreportedCapabilityWork)) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = "unreported-capability-work",
                "gateway.session.failed"
            );
            (
                if is_follow {
                    Terminal::Answered(OutboundReply::text(UNREPORTED_WORK_REPLY.to_owned()))
                } else {
                    Terminal::Failed(UNREPORTED_WORK_REPLY.to_owned())
                },
                MessageOutcome::Failed,
                None,
            )
        }
        Err(error) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category(),
                error = %error,
                "gateway.session.failed"
            );
            let terminal = match error {
                SessionError::Model(InferenceError::OverBudget(_)) => {
                    Terminal::Failed(bound_outbound(liveness.templates.failed()))
                }
                SessionError::Model(error) | SessionError::Prompt(PromptError::Model(error)) => {
                    if is_follow {
                        Terminal::Silent
                    } else {
                        remember_stop(runner, &key, StopCause::Model(error.kind()))
                    }
                }
                SessionError::Prompt(PromptError::EmptyAnswer) => {
                    if is_follow {
                        Terminal::Silent
                    } else {
                        remember_stop(runner, &key, StopCause::EmptyAnswer)
                    }
                }
                SessionError::Prompt(PromptError::MaxSteps { .. }) => {
                    if is_follow {
                        Terminal::Silent
                    } else {
                        remember_stop(runner, &key, StopCause::MaxSteps)
                    }
                }
                SessionError::BrokerClient(_)
                | SessionError::BrokerLeg(_)
                | SessionError::TransportId(_)
                | SessionError::ModelCredential(_)
                | SessionError::ProxyOnlyModel { .. }
                | SessionError::Prompt(
                    PromptError::Cancelled
                    | PromptError::ZeroSteps
                    | PromptError::UnknownTool(_)
                    | PromptError::TooManyToolCalls { .. }
                    | PromptError::EmptyToolCallId
                    | PromptError::InvalidArguments { .. }
                    | PromptError::ArgumentsNotObject { .. }
                    | PromptError::AgentConfigArgumentsNotEmpty { .. }
                    | PromptError::DeclineReplyArgumentsNotEmpty { .. }
                    | PromptError::MissingCommand { .. }
                    | PromptError::MissingAssetId { .. }
                    | PromptError::MissingSkillName { .. }
                    | PromptError::UnexpectedSkillArguments { .. }
                    | PromptError::InvalidSuggestion { .. }
                    | PromptError::InvalidWake { .. }
                    | PromptError::UnreportedCapabilityWork,
                ) => Terminal::Failed(bound_outbound(liveness.templates.failed())),
            };
            (terminal, MessageOutcome::Failed, None)
        }
    };
    let delivered = progress.terminal(terminal).await;
    if delivered {
        reply_delivered(completed_outcome);
    }
    attachments.finish(match (&outcome, delivered) {
        (Ok(_), true) => AssetDeliveryDisposition::Delivered,
        (Ok(_), false) => AssetDeliveryDisposition::Failed,
        (Err(_), _) => AssetDeliveryDisposition::Abandoned,
    });
    if delivered {
        let answered = match completed_outcome {
            MessageOutcome::Answered => true,
            MessageOutcome::Declined
            | MessageOutcome::Refused
            | MessageOutcome::Failed
            | MessageOutcome::ReplyFailed
            | MessageOutcome::Cancelled
            | MessageOutcome::Sealed
            | MessageOutcome::Unauthorized
            | MessageOutcome::FollowInactive
            | MessageOutcome::Steered
            | MessageOutcome::Queued
            | MessageOutcome::Busy
            | MessageOutcome::RouteLost => false,
        };
        if answered
            && leave_request.load(std::sync::atomic::Ordering::Acquire)
            && let Some(table) = runner.follow.as_ref()
        {
            let table = Arc::clone(table);
            let follow_key = crate::follow::follow_key(
                &route.transport,
                route.agent.as_str(),
                &route.conversation,
                &message.conversation.key(),
            );
            let index = route.route_index;
            match tokio::task::spawn_blocking(move || table.leave(&follow_key)).await {
                Ok(Ok(())) => tracing::info!(
                    audit.event = "gateway.follow",
                    operation = "self-left",
                    route = index,
                    "gateway.follow"
                ),
                _ => tracing::warn!(
                    audit.event = "gateway.follow",
                    operation = "refused",
                    route = index,
                    reason = "persistence",
                    "gateway.follow"
                ),
            }
        }
        if is_follow
            && answered
            && !leave_request.load(std::sync::atomic::Ordering::Acquire)
            && let Some(table) = runner.follow.as_ref()
        {
            let table = Arc::clone(table);
            let follow_key = crate::follow::follow_key(
                &route.transport,
                route.agent.as_str(),
                &route.conversation,
                &message.conversation.key(),
            );
            let index = route.route_index;
            match tokio::task::spawn_blocking(move || table.refresh(&follow_key, SystemTime::now()))
                .await
            {
                Ok(Ok(())) => tracing::info!(
                    audit.event = "gateway.follow",
                    operation = "refreshed",
                    route = index,
                    "gateway.follow"
                ),
                _ => tracing::warn!(
                    audit.event = "gateway.follow",
                    operation = "refused",
                    route = index,
                    reason = "persistence",
                    "gateway.follow"
                ),
            }
        }
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
        Ended {
            outcome: completed_outcome,
            journal: pending_journal,
        }
    } else {
        Ended {
            outcome: MessageOutcome::ReplyFailed,
            journal: pending_journal,
        }
    }
}

async fn reject_sealed_thread(
    runner: &SessionRunner,
    message: &InboundMessage,
    driver: &Arc<dyn ChatDriver>,
    sealed: Option<SealedConversation>,
) -> bool {
    match message.conversation.kind {
        ConversationKind::Thread if sealed.is_some() => {
            revoke_thread_ownership(runner, message);
            tracing::info!(
                event = "gateway_session_rejected",
                reason = "sealed",
                "gateway.session.rejected"
            );
            if !matches!(message.message_id, MessageId::Follow(_))
                && !message
                    .thread_continuation
                    .as_ref()
                    .is_some_and(|continuation| continuation.inherited)
                && answer(driver, message, SEALED_THREAD_REPLY).await
            {
                reply_delivered(MessageOutcome::Sealed);
            }
            return true;
        }
        ConversationKind::Thread
        | ConversationKind::DirectMessage
        | ConversationKind::GroupDirectMessage
        | ConversationKind::Channel => {}
    }
    false
}

async fn sealed_conversation(
    runner: &SessionRunner,
    key: &ConversationKey,
    forget_after: Duration,
) -> Option<SealedConversation> {
    let Some(journal) = runner.journal.as_ref() else {
        return runner.sealed_conversations.lock().get(key).copied();
    };
    let journal = Arc::clone(journal);
    let stem = key.journal_stem();
    let result = tokio::task::spawn_blocking(move || journal.sealed(&stem, forget_after)).await;
    let reason = match result {
        Ok(Ok(sealed)) => return sealed,
        Ok(Err(error)) => error.label(),
        Err(_) => "task",
    };
    tracing::warn!(
        event = "gateway_recall_failed",
        source = "seal",
        reason,
        "gateway.recall.failed"
    );
    None
}

pub(crate) async fn seal_conversation(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: &InboundMessage,
    key: &ConversationKey,
) -> bool {
    if route.follow
        && let Some(table) = runner.follow.as_ref()
    {
        let table = Arc::clone(table);
        let follow_key = crate::follow::follow_key(
            &route.transport,
            route.agent.as_str(),
            &route.conversation,
            &message.conversation.key(),
        );
        let left = tokio::task::spawn_blocking(move || table.leave(&follow_key)).await;
        match left {
            Ok(Ok(())) => tracing::info!(
                audit.event = "gateway.follow",
                operation = "sealed",
                route = route.route_index,
                "gateway.follow"
            ),
            Ok(Err(error)) => tracing::warn!(
                audit.event = "gateway.follow",
                operation = "refused",
                route = route.route_index,
                reason = error.label(),
                "gateway.follow"
            ),
            Err(_) => tracing::warn!(
                audit.event = "gateway.follow",
                operation = "refused",
                route = route.route_index,
                reason = "task",
                "gateway.follow"
            ),
        }
    }
    let seal = SealedConversation {
        at: SystemTime::now(),
    };
    let Some(journal) = runner.journal.as_ref() else {
        runner.sealed_conversations.lock().insert(key.clone(), seal);
        return true;
    };
    let journal = Arc::clone(journal);
    let stem = key.journal_stem();
    let result = tokio::task::spawn_blocking(move || journal.seal(&stem, seal.at)).await;
    let reason = match result {
        Ok(Ok(())) => return true,
        Ok(Err(error)) => error.label(),
        Err(_) => "task",
    };
    tracing::warn!(
        event = "gateway_journal_seal_failed",
        reason,
        "gateway.journal.seal.failed"
    );
    false
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
    route: &BoundRoute,
    key: &ConversationKey,
    message: &InboundMessage,
    progress: &mut ProgressPolicy,
    cancellation: &SessionCancellation,
) -> MessageOutcome {
    tracing::info!(
        event = "gateway_session_cancelled",
        "gateway.session.cancelled"
    );
    let by = cancellation.source().unwrap_or(CancelSource::Operator);
    let sealed = match by {
        CancelSource::Budget {
            limit: BudgetLimit::WallClock,
        } => {
            let sealed = seal_conversation(runner, route, message, key).await;
            runner.conversations.remove(key, EvictionReason::Sealed);
            sealed
        }
        CancelSource::User { .. } | CancelSource::Operator => false,
    };
    let terminal = if sealed && message.conversation.kind == ConversationKind::Thread {
        revoke_thread_ownership(runner, message);
        Terminal::Stopped(StopCause::Cancelled(by))
    } else {
        remember_stop(runner, key, StopCause::Cancelled(by))
    };
    if progress.terminal(terminal).await {
        reply_delivered(MessageOutcome::Cancelled);
    }
    MessageOutcome::Cancelled
}

pub(crate) fn reply_delivered(outcome: MessageOutcome) {
    tracing::info!(
        target: "dekopon_gatewayd::audit",
        { audit.event = "reply.delivered", outcome = outcome.label() },
        "reply.delivered"
    );
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
            delivery: None,
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
        tracing::debug!(
            event = "gateway_memory_record_skipped",
            reason = "wake",
            "gateway.memory.record.skipped"
        );
        return;
    };
    let Some(delivery) = delivery_identity(message, message_id, &claim) else {
        tracing::warn!(
            event = "gateway_memory_record_failed",
            category = "delivery-identity",
            "gateway.memory.record.failed"
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
            "gateway.memory.record.failed"
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
        dekopon_broker_protocol::ChatTransportKind::Github => None,
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
            tracing::error!(
                event = "gateway_reply_failed",
                category = error.category(),
                "gateway.reply.failed"
            );
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

    #[tokio::test(start_paused = true)]
    async fn stranger_replies_share_one_global_slot_and_expire_without_a_backlog() {
        let gate = SessionGate::new(4);
        assert!(gate.stranger_reply());
        assert!(!gate.clone().stranger_reply());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(gate.clone().stranger_reply());
        assert!(!gate.stranger_reply());
    }

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
    fn the_real_factory_shares_a_bound_clients_pool_only_within_its_generation() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let factory = ConfiguredModels::default();
        let one = Arc::new(compatible("one"));
        let two = Arc::new(compatible("two"));
        let bind = |config: &Arc<ModelConfig>| {
            factory
                .build_bound(
                    config,
                    runtime.handle().clone(),
                    tokio::sync::watch::channel(false).1,
                )
                .unwrap()
        };
        let first = bind(&one);
        let first_pool = factory.bound_client_for_test(&one).unwrap();
        let again = bind(&one);
        let other = bind(&two);
        assert!(!Arc::ptr_eq(&first, &again));
        assert!(Arc::ptr_eq(
            &first_pool,
            &factory.bound_client_for_test(&one).unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &first_pool,
            &factory.bound_client_for_test(&two).unwrap()
        ));
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(factory.bound_clients.lock().len(), 2);
    }

    #[test]
    fn a_failed_client_build_is_not_cached_and_codex_bridges_are_per_session() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let factory = ConfiguredModels::default();
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("auth.json");
        let config = Arc::new(serde_json::from_value::<ModelConfig>(serde_json::json!({"kind":"chatgptSubscription", "name":"codex", "model":"fixture", "timeoutMs":2000, "authFile":path})).unwrap());
        let bind = || {
            factory.build_bound(
                &config,
                runtime.handle().clone(),
                tokio::sync::watch::channel(false).1,
            )
        };
        assert!(matches!(
            bind(),
            Err(SessionError::Model(InferenceError::Authentication(_)))
        ));
        assert!(factory.bound_clients.lock().is_empty());
        std::fs::write(path, serde_json::to_vec(&serde_json::json!({"version":1,"access":"synthetic","refresh":"synthetic","expiresAt":u64::MAX,"accountId":"synthetic"})).unwrap()).unwrap();
        let first = bind().unwrap();
        let again = bind().unwrap();
        assert!(!Arc::ptr_eq(&first, &again));
        let client = factory.bound_client_for_test(&config).unwrap();
        assert_eq!(factory.bound_clients.lock().len(), 1);
        assert!(matches!(client.as_ref(), ModelClient::Codex(_)));
    }

    #[test]
    fn two_model_generations_keep_one_credential_owner_and_distinct_same_name_clients() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let factory = ConfiguredModels::default();
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("auth.json");
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"version":1,"access":"synthetic","refresh":"synthetic","expiresAt":u64::MAX,"accountId":"synthetic"})).unwrap()).unwrap();
        let config = |model: &str| {
            Arc::new(serde_json::from_value::<ModelConfig>(serde_json::json!({"kind":"chatgptSubscription", "name":"codex", "model":model, "timeoutMs":2000, "authFile":path})).unwrap())
        };
        let first = config("old");
        let second = config("new");
        let third = config("newer");
        let credential = factory
            .chatgpt_credential(Some(&path), Duration::from_secs(2))
            .unwrap();
        for model in [&first, &second, &third, &first] {
            factory
                .build_bound(
                    model,
                    runtime.handle().clone(),
                    tokio::sync::watch::channel(false).1,
                )
                .unwrap();
        }
        assert_eq!(factory.bound_clients.lock().len(), 3);
        assert_eq!(factory.chatgpt_credentials.lock().len(), 1);
        assert!(Arc::ptr_eq(
            &credential,
            &factory
                .chatgpt_credential(Some(&path), Duration::from_secs(2))
                .unwrap()
        ));
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"version":1,"access":"rotated-synthetic","refresh":"rotated-synthetic","expiresAt":u64::MAX,"accountId":"synthetic"})).unwrap()).unwrap();
        let adopted = credential
            .force_refresh(&dekopon_core::Redacted::new("synthetic".to_owned()))
            .unwrap();
        assert_eq!(
            adopted.refresh,
            Some(dekopon_model::chatgpt::RefreshOutcome::Adopted)
        );
        assert_eq!(
            factory
                .chatgpt_credential(Some(&path), Duration::from_secs(2))
                .unwrap()
                .current()
                .unwrap()
                .refresh,
            None
        );
        let clients = factory.bound_clients.lock();
        assert!(!Arc::ptr_eq(
            &clients[&(Arc::as_ptr(&first) as usize)].1,
            &clients[&(Arc::as_ptr(&second) as usize)].1
        ));
    }

    #[test]
    fn retired_generations_release_route_and_memory_clients_after_old_work_finishes() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let factory = ConfiguredModels::default();
        let config = |name: &str| Arc::new(compatible(name));
        let first = config("first");
        let memory = compatible("memory");
        let old = Arc::downgrade(&first);
        let bind = |anchor: &Arc<ModelConfig>| {
            factory
                .build_bound(
                    anchor,
                    runtime.handle().clone(),
                    tokio::sync::watch::channel(false).1,
                )
                .unwrap();
            factory
                .build_anchored(
                    anchor,
                    &memory,
                    runtime.handle().clone(),
                    tokio::sync::watch::channel(false).1,
                )
                .unwrap();
        };
        bind(&first);
        let route_client = Arc::downgrade(&factory.bound_client_for_test(&first).unwrap());
        let memory_client = Arc::downgrade(
            &factory.anchored_clients.lock()[&(Arc::as_ptr(&first) as usize)].1[0].1,
        );
        for name in ["second", "third"] {
            let next = config(name);
            bind(&next);
            factory.retire();
            assert!(route_client.upgrade().is_some());
            assert!(memory_client.upgrade().is_some());
        }
        drop(first);
        factory.retire();
        assert!(old.upgrade().is_none());
        assert!(route_client.upgrade().is_none());
        assert!(memory_client.upgrade().is_none());
        assert!(factory.bound_clients.lock().is_empty());
        assert!(factory.anchored_clients.lock().is_empty());
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
            let config = Arc::new(serde_json::from_value::<ModelConfig>(serde_json::json!({"kind":"chatgptSubscription", "name":name, "model":"fixture", "timeoutMs":2000, "authFile":path})).unwrap());
            factory
                .build_bound(
                    &config,
                    runtime.handle().clone(),
                    tokio::sync::watch::channel(false).1,
                )
                .unwrap();
        }
        assert_eq!(
            factory.chatgpt_credentials.lock().len(),
            2,
            "one credential per auth file, not per model"
        );
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TaskFailure {
    #[error("the session task panicked: {cause}")]
    Panicked { cause: String },
    #[error("the session task was cancelled")]
    Cancelled,
}

impl TaskFailure {
    const fn category(&self) -> &'static str {
        match self {
            Self::Panicked { .. } => "panicked",
            Self::Cancelled => "cancelled",
        }
    }
}

impl From<tokio::task::JoinError> for TaskFailure {
    fn from(error: tokio::task::JoinError) -> Self {
        match error.try_into_panic() {
            Ok(payload) => Self::Panicked {
                cause: payload
                    .downcast_ref::<&str>()
                    .map(|cause| (*cause).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default(),
            },
            Err(_cancelled) => Self::Cancelled,
        }
    }
}

#[cfg(test)]
mod inventory_prompt_tests {
    use super::*;
    use crate::asset::{AssetSourceRef, PendingAsset};
    use crate::transport::MAX_INBOUND_TEXT_BYTES;

    fn registered() -> asset::Registered {
        let key = ConversationKey::private(
            &"reviewer".parse().unwrap(),
            "dev",
            "fitted",
            &"tel.15550000001".parse().unwrap(),
        );
        let store = AssetStore::new(8, crate::ONE_SHOT_ASSET_IDLE);
        store.assets_for(
            &key,
            (1..=200)
                .map(|id| PendingAsset {
                    name: format!("file-{id}.png"),
                    mime: "image/png".into(),
                    size: Some(1024),
                    source: Some(AssetSourceRef::Telegram {
                        file_id: id.to_string(),
                    }),
                })
                .collect(),
            Instant::now(),
        )
    }

    fn check(path: InventoryPath, empty: bool) {
        let registered = registered();
        let principal = "mapped-participant-with-a-longer-name".parse().unwrap();
        let delivery = "[gateway: a previous asset send did not complete. Sent flags remain set; no automatic retry was made.]";
        let stop = StopCause::MaxSteps.notice();
        let text = if empty {
            String::new()
        } else {
            "x".repeat(15 * 1024)
        };
        let text = match path {
            InventoryPath::Steer => {
                format!("[gateway: sent while you were working, +123s]\n{text}")
            }
            InventoryPath::Turn | InventoryPath::Follow => text,
        };
        let prompt = PromptFrame {
            inventory: None,
            principal: Some(&principal),
            delivery: Some(delivery),
            stop: Some(&stop),
        }
        .inventory(&text, &registered, true, asset::TextReader::AssetCat, path);
        assert_eq!(bound_inbound(&prompt), prompt);
        assert!(prompt.len() <= MAX_INBOUND_TEXT_BYTES);
        assert!(!prompt.contains("[message truncated by the gateway]"));
        assert!(prompt.starts_with(
            "[gateway: authenticated participant: mapped-participant-with-a-longer-name]\n"
        ));
        assert!(prompt.contains(
            "chat-asset:200 — file-200.png (image/png, 1 KB) — attached to this message"
        ));
        assert!(
            prompt.ends_with(
                "Call fetch_chat_asset with the number to look at an image or document.]"
            )
        );
        match path {
            InventoryPath::Turn => {
                assert!(prompt.contains(delivery));
                assert!(prompt.contains(&stop));
            }
            InventoryPath::Follow => {
                assert!(!prompt.contains(delivery));
                assert!(prompt.contains(&stop));
            }
            InventoryPath::Steer => {
                assert!(prompt.contains("[gateway: sent while you were working, +123s]"));
                assert!(!prompt.contains(delivery));
                assert!(!prompt.contains(&stop));
            }
        }
    }

    #[test]
    fn a_message_near_the_limit_keeps_arrivals_in_a_shrunken_note() {
        let mut registered = registered();
        registered.arrived = vec![200];
        let principal = "mapped-participant".parse().unwrap();
        let expected_note = "\n  chat-asset:200 — file-200.png (image/png, 1 KB) — attached to this message\n  199 older files, chat-asset:1 to chat-asset:199, are not listed for space; each number still works.";
        let text = "x".repeat(
            MAX_INBOUND_TEXT_BYTES
                - attribution_header(Some(&principal)).len()
                - 2
                - expected_note.len(),
        );
        let prompt = PromptFrame {
            inventory: None,
            principal: Some(&principal),
            delivery: None,
            stop: None,
        }
        .inventory(
            &text,
            &registered,
            true,
            asset::TextReader::AssetCat,
            InventoryPath::Turn,
        );
        assert_eq!(
            prompt,
            format!(
                "{}{}\n\n{expected_note}",
                attribution_header(Some(&principal)),
                text
            )
        );
        assert_eq!(bound_inbound(&prompt), prompt);
    }

    #[test]
    fn a_turn_note_survives_notices_and_mapped_attribution_without_a_cut() {
        check(InventoryPath::Turn, false);
    }
    #[test]
    fn a_follow_note_survives_notices_and_mapped_attribution_without_a_cut() {
        check(InventoryPath::Follow, false);
    }
    #[test]
    fn a_steer_note_survives_its_header_and_mapped_attribution_without_a_cut() {
        check(InventoryPath::Steer, false);
    }
    #[test]
    fn an_empty_text_note_survives_notices_and_mapped_attribution_without_a_cut() {
        check(InventoryPath::Turn, true);
    }
}
