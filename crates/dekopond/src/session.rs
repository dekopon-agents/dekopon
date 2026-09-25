//! A session holds no authority of its own; it opens an attested broker leg naming the sender, and
//! an empty grant ends it before any model token is spent, whatever the message text says.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, hash_map::Entry},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use dekopon_agent::{
    BrokerLeg, BrokerLegError, CancelSource, IdSequence, ProgressEvent, ProgressSink, ShellRuntime,
    attachment::{AssetDeliveryDisposition, ChatAssetInputs, ReplyAttachments},
    meta::{AgentConfigView, MemoryConfigView, MemoryScopeView, SessionConfigView, SkillView},
    prompt::{
        CancellationProbe, ConversationTurn, History, PromptError, ReplyDisposition, SessionInputs,
        run_prompt_session,
    },
};
use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatScopeClaim, ClientError, DeliveredTurnRequest, DeliveryIdentity,
    ERROR_STORAGE_BUSY, ERROR_STORAGE_CORRUPT, ERROR_STORAGE_IO, ERROR_STORAGE_QUOTA,
    ERROR_STORAGE_TIMEOUT, ERROR_UNAUTHENTICATED, InvocationOutcome, InvocationResult, Trigger,
};
use dekopon_model::error::InferenceError;
use dekopon_model::{
    blocking::BlockingModel,
    codex::CodexClient,
    inference::ModelClient,
    model::{ChatModel, CompletionOptions},
    openai::OpenAiClient,
};
use dekopon_process::{CancelHandle, CancelSignal};
use dekopon_shell::{CapabilityInvoker as _, Limits as ShellLimits};
use thiserror::Error;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tracing::Instrument as _;

use crate::{
    asset::{self, AssetAccess, AssetStore, RecalledAsset, SessionAssets},
    config::{
        MemoryPolicy, MemoryScope, MemoryWindow, ModelConfig, RecallSource, ResolvedBroker,
        ResolvedLiveness,
    },
    conversation::{ConversationKey, ConversationSeed, ConversationStore, EvictionReason},
    journal::{self, Journal},
    progress::{ProgressInputs, ProgressPolicy, Terminal},
    routes::BoundRoute,
    transport::{
        AssetFetcher, CancelRequest, ChatDriver, InboundMessage, OutboundReply, PastMessage,
        ThreadOwnership, TransportError, bound_inbound, bound_outbound, credential_from,
    },
};

pub(crate) const UNAUTHORIZED_REPLY: &str = "You're not authorized to use this agent.";
pub(crate) const BUSY_REPLY: &str = "I'm busy — try again shortly.";
pub(crate) const FAILURE_REPLY: &str = "The agent could not complete this request.";
pub(crate) const UNREPORTED_WORK_REPLY: &str = "The agent attempted capability work but could not report the result. Check the audit before retrying.";
pub(crate) const STOPPED_REPLY: &str = "Stopped.";
pub(crate) const EMPTY_REPLY: &str = "[empty response]";

const PLATFORM_RECALL_MAX_MESSAGES: usize = 100;
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
        ModelConfig::Openrouter { api_key_env, .. } => Some(api_key_env.as_str()),
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
    fn chatgpt_credential(
        &self,
        auth_file: Option<&std::path::Path>,
        timeout: std::time::Duration,
    ) -> Result<Arc<dekopon_model::chatgpt::CredentialFile>, InferenceError> {
        use dekopon_model::{chatgpt, error::AuthError};
        let path = chatgpt::resolve_auth_path(auth_file).map_err(AuthError::Credential)?;
        let cached = self
            .chatgpt_credentials
            .lock()
            .expect("gateway chatgpt credentials")
            .get(&path)
            .cloned();
        if let Some(credential) = cached {
            return Ok(credential);
        }
        // Opened outside the lock; if two models race to open the same file, the first instance
        // wins and is the one handed to every later model.
        let opened =
            Arc::new(chatgpt::CredentialFile::open(&path, timeout).map_err(AuthError::Credential)?);
        let mut credentials = self
            .chatgpt_credentials
            .lock()
            .expect("gateway chatgpt credentials");
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
        let cached = self
            .clients
            .lock()
            .expect("gateway model clients")
            .get(model.name())
            .cloned();
        let client = match cached {
            Some(client) => client,
            None => {
                let built = self.construct(model)?;
                let mut clients = self.clients.lock().expect("gateway model clients");
                Arc::clone(clients.entry(model.name().to_owned()).or_insert(built))
            }
        };
        let timeout_ms = match model {
            ModelConfig::ChatgptSubscription { timeout_ms, .. }
            | ModelConfig::OpenaiCompatible { timeout_ms, .. }
            | ModelConfig::Openrouter { timeout_ms, .. } => *timeout_ms,
        };
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

pub(crate) struct SessionGate {
    permits: Arc<Semaphore>,
    late_permits: Arc<Semaphore>,
    refusals: Arc<Semaphore>,
    in_flight: Arc<Mutex<BTreeSet<AdmissionKey>>>,
}

impl SessionGate {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent)),
            late_permits: Arc::new(Semaphore::new(max_concurrent)),
            refusals: Arc::new(Semaphore::new(max_concurrent)),
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    pub fn admit(&self, key: AdmissionKey) -> Option<SessionAdmission> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().ok()?;
        let mut in_flight = self.in_flight.lock().expect("session in-flight registry");
        if !in_flight.insert(key.clone()) {
            return None;
        }
        drop(in_flight);
        Some(SessionAdmission {
            _permit: permit,
            key,
            in_flight: Arc::clone(&self.in_flight),
        })
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
    in_flight: Arc<Mutex<BTreeSet<AdmissionKey>>>,
}

impl Drop for SessionAdmission {
    fn drop(&mut self) {
        self.in_flight
            .lock()
            .expect("session in-flight registry")
            .remove(&self.key);
    }
}

#[derive(Clone)]
pub(crate) struct SessionCancellation {
    state: Arc<AtomicU8>,
    source: Arc<Mutex<Option<CancelSource>>>,
    woken: Arc<Notify>,
    handle: CancelHandle,
    signal: CancelSignal,
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
            let mut recorded = self.source.lock().expect("session cancellation source");
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
            self.woken.notify_waiters();
        }
        cancelled
    }

    pub(crate) fn source(&self) -> Option<CancelSource> {
        *self.source.lock().expect("session cancellation source")
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

mod late_photos;
pub(crate) use late_photos::{LatePhotoReceipt, LatePhotos};

type ActiveSessionKey = (String, String);

#[derive(Clone)]
struct ActiveSession {
    started_at: tokio::time::Instant,
    late_photos: LatePhotos,
    subject: dekopon_core::ExternalSubject,
    cancellation: SessionCancellation,
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

#[derive(Clone)]
pub(crate) struct ActiveSessions {
    entries: Arc<Mutex<HashMap<ActiveSessionKey, ActiveSession>>>,
    recent: Arc<Mutex<late_photos::RecentSessions>>,
    intakes: late_photos::LateIntakes,
}

impl ActiveSessions {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: Arc::default(),
            intakes: late_photos::LateIntakes::default(),
            recent: Arc::new(Mutex::new(late_photos::RecentSessions::new(capacity))),
        }
    }

    fn register(
        &self,
        message: &InboundMessage,
        route: &BoundRoute,
        cancellation: SessionCancellation,
    ) -> ActiveRegistration {
        let key = (message.transport.clone(), message.conversation.key());
        let late_photos = LatePhotos::new(route, message, cancellation.clone());
        let session = ActiveSession {
            started_at: tokio::time::Instant::now(),
            late_photos: late_photos.clone(),
            subject: message.subject.clone(),
            cancellation: cancellation.clone(),
        };
        let registered = match self
            .entries
            .lock()
            .expect("active session registry")
            .entry(key.clone())
        {
            Entry::Vacant(entry) => {
                entry.insert(session);
                true
            }
            Entry::Occupied(_) => {
                tracing::error!(event = "gateway_session_registry_conflict");
                false
            }
        };
        ActiveRegistration {
            recent: Arc::clone(&self.recent),
            late_photos,
            entries: Arc::clone(&self.entries),
            key,
            cancellation,
            registered,
        }
    }

    pub(crate) fn cancel(&self, request: &CancelRequest) -> CancelOutcome {
        let execution = self.cancel_execution(request);
        self.intakes.cancel(request, execution)
    }

    fn cancel_execution(&self, request: &CancelRequest) -> CancelOutcome {
        let key = (request.transport.clone(), request.conversation_id.clone());
        let Some(session) = self
            .entries
            .lock()
            .expect("active session registry")
            .get(&key)
            .cloned()
        else {
            return CancelOutcome::NoSession;
        };
        if session.subject.canonical() != request.subject {
            return CancelOutcome::OtherSubject;
        }
        if session
            .cancellation
            .cancel(CancelSource::User { via: request.via })
        {
            CancelOutcome::Cancelled
        } else if session.cancellation.is_cancelled() {
            CancelOutcome::AlreadyCancelled
        } else {
            CancelOutcome::Completing
        }
    }
}

struct ActiveRegistration {
    recent: Arc<Mutex<late_photos::RecentSessions>>,
    late_photos: LatePhotos,
    entries: Arc<Mutex<HashMap<ActiveSessionKey, ActiveSession>>>,
    key: ActiveSessionKey,
    cancellation: SessionCancellation,
    registered: bool,
}

impl Drop for ActiveRegistration {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        let mut entries = self.entries.lock().expect("active session registry");
        if entries.get(&self.key).is_some_and(|session| {
            Arc::ptr_eq(&session.cancellation.state, &self.cancellation.state)
        }) && let Some(session) = entries.remove(&self.key)
        {
            self.recent
                .lock()
                .expect("recent session registry")
                .complete(self.key.clone(), session);
        }
    }
}

pub(crate) struct SessionRunner {
    pub broker: ResolvedBroker,
    pub models: Arc<ModelCache>,
    pub gate: SessionGate,
    pub reply_on_busy: bool,
    pub conversations: ConversationStore,
    pub journal: Option<Arc<Journal>>,
    pub assets: Arc<AssetStore>,
    pub asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>>,
    pub liveness: BTreeMap<String, Arc<ResolvedLiveness>>,
    pub thread_ownership: HashMap<String, Arc<dyn ThreadOwnership>>,
    pub active_sessions: ActiveSessions,
}

struct RecalledWindow {
    history: History,
    assets: Vec<RecalledAsset>,
    next_asset_id: u64,
}

async fn recall_window(
    runner: &SessionRunner,
    driver: &dyn ChatDriver,
    message: &InboundMessage,
    key: &ConversationKey,
    granted: &[String],
    window: MemoryWindow,
) -> Option<RecalledWindow> {
    let span = tracing::Span::current();
    match window.recall {
        RecallSource::None => None,
        RecallSource::Journal => {
            let journal = Arc::clone(runner.journal.as_ref()?);
            let stem = key.journal_stem();
            let grant = journal::grant_digest(granted);
            let recalled = tokio::task::spawn_blocking(move || {
                journal.recall(&stem, &grant, window, SystemTime::now())
            })
            .await;
            match recalled {
                Ok(Ok(recalled)) => {
                    span.record("conversation.recall_source", "journal");
                    Some(RecalledWindow {
                        history: recalled.history,
                        assets: recalled.assets,
                        next_asset_id: recalled.next_asset_id,
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
            let history = driver.history()?;
            let limit = window
                .limits
                .max_turns
                .saturating_mul(2)
                .min(PLATFORM_RECALL_MAX_MESSAGES);
            let read = tokio::time::timeout(
                PLATFORM_RECALL_TIMEOUT,
                history.recent(&message.conversation, &message.message_id, limit),
            )
            .await;
            let reason = match read {
                Ok(Ok(messages)) => {
                    span.record("conversation.recall_source", "platform");
                    return Some(platform_window(messages, window, SystemTime::now()));
                }
                Ok(Err(error)) => error.category(),
                Err(_) => "timeout",
            };
            tracing::warn!(event = "gateway_recall_failed", source = "platform", reason);
            None
        }
    }
}

// Authors come from the chat service, not the broker, so the label does not claim authentication.
fn platform_window(
    messages: Vec<PastMessage>,
    window: MemoryWindow,
    now: SystemTime,
) -> RecalledWindow {
    let horizon = now
        .checked_sub(window.forget_after)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut turns = Vec::new();
    let mut assets = Vec::new();
    let mut pending_user = String::new();
    let mut next_asset_id = 1;
    // Without Discord's Message Content intent other people's messages arrive with no content.
    for message in messages
        .into_iter()
        .filter(|message| message.at >= horizon)
        .filter(|message| !message.text.trim().is_empty() || !message.assets.is_empty())
    {
        let mut text = message.text;
        for asset in message.assets {
            text.push_str(&format!(
                "\n[gateway: attached Chat Asset #{next_asset_id} — {}]",
                asset.name
            ));
            assets.push(RecalledAsset {
                id: next_asset_id,
                asset,
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
            pending_user.push_str(&format!(
                "[gateway: chat history, from {}]\n{text}",
                message.author
            ));
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
    }
}

async fn append_journal(
    journal: &Arc<Journal>,
    key: &ConversationKey,
    granted: &[String],
    window: MemoryWindow,
    turn: ConversationTurn,
    inventory: Vec<asset::AssetRef>,
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

/// Only the canonical subject line is attribution the gateway vouches for; the rest is untrusted
/// user text that can contain lookalike labels or prompt injection, even under the local-dev
/// transport.
fn attributed_prompt(subject: &dekopon_core::ExternalSubject, text: &str) -> String {
    bound_inbound(&format!(
        "[gateway: authenticated participant: {}]\n{text}",
        subject.canonical()
    ))
}

pub(crate) fn run_session(
    runner: Arc<SessionRunner>,
    route: BoundRoute,
    mut message: InboundMessage,
    driver: Arc<dyn ChatDriver>,
) -> impl std::future::Future<Output = ()> + Send {
    let receipts = crate::collection::Dispositions(message.constituents.clone());
    let intake = message.late_photos.as_ref().and_then(|_| {
        runner
            .active_sessions
            .intakes
            .register(&runner.gate, &message)
    });
    async move {
        let span = {
            let received = std::mem::replace(&mut message.receive_span, tracing::Span::none());
            tracing::info_span!(
                parent: &received,
                "gateway.message",
                transport = %message.transport,
                agent = %route.agent,
                outcome = tracing::field::Empty,
                batch.members = tracing::field::Empty
            )
        };
        span.record("batch.members", receipts.0.len());
        for receipt in &receipts.0 {
            dekopon_telemetry::link_span(&span, receipt);
        }
        let outcome = execute(runner, route, message, driver, intake)
            .instrument(span.clone())
            .await;
        span.record("outcome", outcome);
        receipts.finish(outcome);
    }
}

async fn execute(
    runner: Arc<SessionRunner>,
    route: BoundRoute,
    message: InboundMessage,
    driver: Arc<dyn ChatDriver>,
    intake: Option<late_photos::LateIntake>,
) -> &'static str {
    if message.constituents.is_empty() {
        crate::collection::record_received(&message);
    }

    if let Some(late) = &message.late_photos {
        let Some(intake) = intake else {
            if late.is_stopped() {
                return "stopped";
            }
            if let Some(_reply) = runner.gate.refusal() {
                answer(&driver, &message, late_photos::REFUSED_REPLY).await;
            }
            return "late-busy";
        };
        return late
            .retain(&runner, &route, &message, &driver, &intake)
            .await;
    }

    // The admission slot, the active-session registry, the cancel request, and the memory key are
    // all keyed on the same conversation key, so a stop can never be filed apart from its session.
    let key = (message.transport.clone(), message.conversation.key());
    let Some(admission) = runner.gate.admit(key) else {
        tracing::info!(event = "gateway_session_rejected", reason = "busy");
        if (runner.reply_on_busy || !message.constituents.is_empty())
            && let Some(_reply) = runner.gate.refusal()
        {
            answer(&driver, &message, BUSY_REPLY).await;
        }
        return "busy";
    };

    if message.transport_kind == dekopon_broker_protocol::ChatTransportKind::Whatsapp
        && route.memory.window().is_some()
    {
        runner
            .conversations
            .invalidate_late_input(&conversation_key(&route, &message));
    }

    let outcome = session(&runner, &route, &message, &driver)
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
    drop(admission);
    outcome
}

async fn session(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: &InboundMessage,
    driver: &Arc<dyn ChatDriver>,
) -> &'static str {
    let cancellation = SessionCancellation::new();
    let _active_registration =
        runner
            .active_sessions
            .register(message, route, cancellation.clone());
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
    // Never cached and never remembered as a permission: this is a fresh answer from the broker
    // about what this subject may reach through this agent, for this message alone.
    let granted = leg.granted();
    // Only trusted route configuration decides whether the subject participates in the state key;
    // message text, transport presentation, and model output never influence that choice.
    let key = conversation_key(route, message);
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
    let (seeded, cache_key, conversation_lease, asset_access, gateway_notice) = match window {
        Some(window) => {
            let recalled = if runner
                .conversations
                .resident(&key, &granted, window, Instant::now())
            {
                None
            } else {
                recall_window(runner, driver.as_ref(), message, &key, &granted, window).await
            };
            let (recalled_history, recalled_assets, next_asset_id) = match recalled {
                Some(recalled) => (
                    Some(recalled.history),
                    recalled.assets,
                    recalled.next_asset_id,
                ),
                None => (None, Vec::new(), 1),
            };
            let ConversationSeed {
                history,
                cache_key,
                assets,
                lease,
                input,
                gateway_notice,
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
            _active_registration
                .late_photos
                .authorized(input, assets.clone(), cache_key.clone());
            (history, cache_key, Some(lease), assets, gateway_notice)
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
        target: "dekopond::audit",
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
    let instructions = match (route.instructions.as_deref(), memory_surface.as_ref()) {
        (Some(instructions), Some(memory)) => {
            Some(format!("{instructions}\n\n{}", memory.prompt_note))
        }
        (None, Some(memory)) => Some(memory.prompt_note.clone()),
        (Some(instructions), None) => Some(instructions.to_owned()),
        (None, None) => None,
    };
    let accepted_types = match &message.reply {
        crate::transport::ReplyTarget::Telegram { .. }
        | crate::transport::ReplyTarget::WhatsApp { .. } => "image/png and image/jpeg",
        _ => "any concrete syntactically valid media type (no wildcards)",
    };
    let instructions = Some(format!(
        "{}\n\n[Gateway assets: this reply adapter accepts {accepted_types}. Plan a converter for other formats; attaching retains a file but only a separately authorized asset.send delivers it. References use chat-asset:<N>, never data URLs.]",
        instructions.as_deref().unwrap_or_default()
    ));
    let images_supported = route.model.accepts_images();
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
    let journal_access = asset_access.clone();
    let text = match runner.assets.take_delivery_notice(&asset_access) {
        Some(note) => bound_inbound(&format!("{note}\n{text}")),
        None => text,
    };
    let text = match gateway_notice {
        Some(notice) => bound_inbound(&format!(
            "[Gateway follow-up previously delivered: {notice}]\n{text}"
        )),
        None => text,
    };
    let text = match window.map(|window| window.scope) {
        Some(MemoryScope::SharedConversation) => attributed_prompt(&message.subject, &text),
        Some(MemoryScope::PrivateConversation) | None => text,
    };
    let assets = Arc::new(SessionAssets::new(
        Arc::clone(&runner.assets),
        asset_access,
        runner.asset_fetchers.get(&message.transport).cloned(),
        tokio::runtime::Handle::current(),
        images_supported,
        registered.fetchable,
    ));
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
    let improvement_suggestions = route.improvement_suggestions;
    let inspect_agent_config = route.inspect_agent_config;
    let session_attachments = Arc::clone(&attachments);
    let progress_sink = Arc::clone(&sink) as Arc<dyn ProgressSink>;
    let leg = leg.with_progress(Arc::clone(&progress_sink), limits.max_capability_calls);
    drop(sink);
    let model_runtime = tokio::runtime::Handle::current();
    let model_cancel = cancellation.signal().watch();
    let result = tokio::task::spawn_blocking(move || {
        let _entered = blocking_span.enter();
        let model = match models.client(&model_config, model_runtime, model_cancel) {
            Ok(model) => model,
            Err(error) => return (Err(error), None, Vec::new()),
        };
        let runtime = ShellRuntime {
            invoker: leg,
            limits: shell,
        };
        let mut history = seeded;
        let mut inputs = SessionInputs::new(&text, limits)
            .with_system(instructions.as_deref())
            .with_skills(&skills)
            .with_options(&options)
            .with_assets(assets.as_ref())
            .with_reply_assets(&session_attachments)
            .with_cancellation(&prompt_cancellation)
            .with_progress(Arc::clone(&progress_sink));
        // Withholding the agent-config view here removes the structured dump but not the underlying
        // instructions, which are still the system prompt, so this is not secrecy from a determined
        // user.
        if inspect_agent_config {
            inputs = inputs.with_agent_config(&agent_config);
        }
        if improvement_suggestions {
            inputs = inputs.with_improvement_suggestions();
        }
        if reply_optional {
            inputs = inputs.with_optional_reply();
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
    .await;

    let (outcome, turn, images) = match result {
        Ok(session) => session,
        Err(_) => {
            if !cancellation.claim_completion() {
                return stopped(&mut progress, &cancellation).await;
            }
            progress.seal();
            tracing::error!(event = "gateway_session_failed", category = "session-task");
            let notice = _active_registration
                .late_photos
                .finish(&runner.assets, false);
            let replied = progress
                .terminal(Terminal::Failed(late_photos::append_notice(
                    liveness.templates.failed(),
                    notice,
                )))
                .await;
            if replied && let Some(notice) = notice {
                _active_registration
                    .late_photos
                    .remember_notice(&runner.conversations, notice);
            }
            return if replied { "failed" } else { "reply-failed" };
        }
    };

    if matches!(&outcome, Err(SessionError::Prompt(PromptError::Cancelled)))
        || cancellation.is_cancelled()
        || !cancellation.claim_completion()
    {
        return stopped(&mut progress, &cancellation).await;
    }

    progress.seal();

    if let Some(window) = window
        && let Some(turn) = turn
        && let Some(lease) = conversation_lease
    {
        let journaled = (window.recall == RecallSource::Journal).then(|| turn.clone());
        if lease.commit(window, turn, &cache_key, Instant::now())
            && let Some(turn) = journaled
            && let Some(journal) = runner.journal.as_ref()
        {
            let inventory = runner.assets.inventory(&journal_access);
            append_journal(journal, &key, &granted, window, turn, inventory).await;
        }
    }

    if matches!(
        &outcome,
        Ok(outcome) if outcome.disposition == ReplyDisposition::Suppress
    ) {
        progress.terminal(Terminal::Silent).await;
        return "declined";
    }

    let late_notice = _active_registration
        .late_photos
        .finish(&runner.assets, outcome.is_ok());
    let (terminal, completed_outcome, delivered_answer) = match &outcome {
        Ok(outcome) => {
            let text = bound_outbound(if outcome.answer.is_empty() && images.is_empty() {
                EMPTY_REPLY
            } else {
                outcome.answer.as_str()
            });
            let text = late_photos::append_notice(&text, late_notice);
            let reply = if images.is_empty() {
                OutboundReply::text(text.clone())
            } else {
                OutboundReply::with_images(text.clone(), images)
            };
            (Terminal::Answered(reply), "answered", Some(text))
        }
        Err(SessionError::Prompt(PromptError::UnreportedCapabilityWork)) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = "unreported-capability-work"
            );
            (
                Terminal::Failed(late_photos::append_notice(
                    UNREPORTED_WORK_REPLY,
                    late_notice,
                )),
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
            (
                Terminal::Failed(late_photos::append_notice(
                    liveness.templates.failed(),
                    late_notice,
                )),
                "failed",
                None,
            )
        }
    };
    let delivered = progress.terminal(terminal).await;
    attachments.finish(match (&outcome, delivered) {
        (Ok(_), true) => AssetDeliveryDisposition::Delivered,
        (Ok(_), false) => AssetDeliveryDisposition::Failed,
        (Err(_), _) => AssetDeliveryDisposition::Abandoned,
    });
    if delivered {
        if let Some(notice) = late_notice {
            _active_registration
                .late_photos
                .remember_notice(&runner.conversations, notice);
        }
        if memory_surface.is_some()
            && let Some(answer) = delivered_answer
            && let Some(claim) = chat_claim
        {
            record_delivered_turn(runner, message, claim, answer).await;
        }
        completed_outcome
    } else {
        "reply-failed"
    }
}

async fn stopped(
    progress: &mut ProgressPolicy,
    cancellation: &SessionCancellation,
) -> &'static str {
    tracing::info!(event = "gateway_session_cancelled");
    let by = cancellation.source().unwrap_or(CancelSource::Operator);
    progress.terminal(Terminal::Cancelled { by }).await;
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
    let client = BrokerClient::new(
        &runner.broker.socket_path,
        runner.broker.server_uid,
        runner.broker.frame,
    )?;
    BrokerLeg::connect(client, Some(chat_claim(route, message)?))
        .await
        .map_err(SessionError::from)
}

/// No normalization step here: the transport already minted the conversation in the canonical form
/// the grant, claim check, and policy engine all compare against, so re-normalizing would create a
/// second definition of the same fact.
fn chat_claim(route: &BoundRoute, message: &InboundMessage) -> Result<Attestation, SessionError> {
    let transport = message
        .transport
        .parse()
        .map_err(SessionError::TransportId)?;
    Ok(Attestation::for_chat(
        message.subject.clone(),
        route.agent.clone(),
        ChatScopeClaim {
            transport,
            kind: message.transport_kind,
            conversation: message.conversation.clone(),
            trigger: Trigger::Message,
        },
    ))
}

async fn record_delivered_turn(
    runner: &SessionRunner,
    message: &InboundMessage,
    claim: Attestation,
    assistant: String,
) {
    let Some(delivery) = delivery_identity(message, &claim) else {
        tracing::warn!(
            event = "gateway_memory_record_failed",
            category = "delivery-identity",
        );
        return;
    };
    let result: Result<(), MemoryRecordFailure> = async {
        let identifiers = IdSequence::for_session();
        let client = BrokerClient::new(
            &runner.broker.socket_path,
            runner.broker.server_uid,
            runner.broker.frame,
        )
        .map_err(|error| MemoryRecordFailure::Broker(BrokerLegError::from(error)))?;
        let result = client
            .record_delivered_turn(
                claim,
                DeliveredTurnRequest {
                    id: identifiers.next_invocation(),
                    trace_parent: identifiers.trace_parent(),
                    delivery,
                    user: message.text.clone(),
                    assistant,
                },
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
    claim: &Attestation,
) -> Option<DeliveryIdentity> {
    let scope = claim.scope.as_ref()?;
    let conversation = &scope.conversation;
    match message.transport_kind {
        dekopon_broker_protocol::ChatTransportKind::Slack => Some(DeliveryIdentity::Slack {
            channel: conversation.id.clone(),
            timestamp: message.message_id.clone(),
        }),
        dekopon_broker_protocol::ChatTransportKind::Discord => Some(DeliveryIdentity::Discord {
            channel: conversation
                .api_channel(dekopon_broker_protocol::ChatTransportKind::Discord)
                .to_owned(),
            message: message.message_id.clone(),
        }),
        dekopon_broker_protocol::ChatTransportKind::Telegram => Some(DeliveryIdentity::Telegram {
            chat: conversation.id.clone(),
            topic: conversation.thread.clone(),
            message: message.message_id.clone(),
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
                message: message.message_id.clone(),
            })
        }
        dekopon_broker_protocol::ChatTransportKind::Local => {
            let mut fields = message.message_id.rsplitn(3, '-');
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
            Some("dedup-capacity") => "dedup-capacity",
            Some("dedup-conflict") => "dedup-conflict",
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
}

impl SessionError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::BrokerClient(_) => "broker-client",
            Self::BrokerLeg(_) => "broker-leg",
            Self::TransportId(_) => "transport-id",
            Self::Model(_) => "model",
            Self::ModelCredential(_) => "model-credential",
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
        let first_pool = Arc::clone(factory.clients.lock().unwrap().get("one").unwrap());
        let again = bind("one");
        let other = bind("two");
        assert!(!Arc::ptr_eq(&first, &again));
        let clients = factory.clients.lock().unwrap();
        assert!(Arc::ptr_eq(&first_pool, clients.get("one").unwrap()));
        assert!(!Arc::ptr_eq(&first_pool, clients.get("two").unwrap()));
        drop(clients);
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(factory.clients.lock().unwrap().len(), 2);
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
        assert!(factory.clients.lock().unwrap().is_empty());
        std::fs::write(path, serde_json::to_vec(&serde_json::json!({"version":1,"access":"synthetic","refresh":"synthetic","expiresAt":u64::MAX,"accountId":"synthetic"})).unwrap()).unwrap();
        let first = bind().unwrap();
        let again = bind().unwrap();
        assert!(!Arc::ptr_eq(&first, &again));
        let cache = factory.clients.lock().unwrap();
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
        assert_eq!(factory.clients.lock().unwrap().len(), 3);
        assert_eq!(
            factory.chatgpt_credentials.lock().unwrap().len(),
            2,
            "one credential per auth file, not per model"
        );
    }
}
