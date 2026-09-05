//! One routed message, from admission through the answer that goes back to chat.
//!
//! A session holds no authority. It opens an *attested* broker leg naming the sender, and whatever
//! that leg reports as granted is what the broker decided the sender may reach through this agent.
//! An empty grant ends the session before a single model token is spent, which is deliberate: the
//! cheapest possible refusal, and one that cannot be talked out of by the message text.

use dekopon_harness::{
    control::{ModelIdentity, ModelRegistry, PreparationError, PreparedModel, SessionControls},
    conversation::{BoundedConversationStore, ConversationKey, ConversationSeed, EvictionReason},
    history::{DeliveryDisposition, JobRecord},
};
use std::{
    collections::{BTreeSet, HashMap, hash_map::Entry},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
    },
    time::Instant,
};

use dekopon_broker_protocol::{
    Attestation, BrokerClient, ChatScopeClaim, ClientError, DeliveredTurnRequest, DeliveryIdentity,
    ERROR_STORAGE_BUSY, ERROR_STORAGE_CORRUPT, ERROR_STORAGE_IO, ERROR_STORAGE_QUOTA,
    ERROR_STORAGE_TIMEOUT, ERROR_UNAUTHENTICATED, InvocationOutcome, InvocationResult,
    ModelUsageReport,
};
use dekopon_harness::{
    bootstrap::SessionBootstrap,
    history::History,
    meta::{AgentConfigView, ConversationConfigView, SessionConfigView, SkillView},
    runtime::{BrokerLeg, BrokerLegError, IdSequence, ShellRuntime, current_trace_parent},
    session::{CancellationProbe, PromptError, ReplyDisposition, SessionEngine},
    tools::GeneratedImageOutput,
};
use dekopon_model::{
    chatgpt::ChatGptCodexModel,
    image::{ImageGenerationError, ImageGenerator, OpenAiImageGenerator},
    model::{ChatModel, CompletionOptions, ModelError, OpenAiChatModel},
};
use dekopon_process::{CancelHandle, CancelSignal};
use dekopon_shell::{CapabilityCallResult, CapabilityInvoker, Limits as ShellLimits};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::Instrument as _;

use crate::{
    activity::{ActivityControl, ActivityLease},
    asset::{self, AssetStore, SessionAssets},
    config::{ConversationPolicy, ImageGeneratorConfig, ModelConfig, ResolvedBroker},
    routes::BoundRoute,
    transport::{
        AssetFetcher, ChatActivity, ChatReplier, DeliveryReceipt, InboundMessage, OutboundReply,
        ReplyTarget, SessionStop, ThreadOwnership, TransportError, bound_inbound, bound_outbound,
        credential_from,
    },
};

/// Trace-identifier prefix, so every broker record a gateway session made is recoverable by prefix.
const TRACE_PREFIX: &str = "dekopond-session";

/// The refusal a subject with no granted capabilities receives.
pub(crate) const UNAUTHORIZED_REPLY: &str = "You're not authorized to use this agent.";
/// The refusal an over-subscribed daemon returns, when configured to answer at all.
pub(crate) const BUSY_REPLY: &str = "I'm busy — try again shortly.";
/// The one thing a failed session ever says.
///
/// Fixed and bounded on purpose. A `PromptError` can carry model-chosen text, a provider message,
/// or a transport diagnostic, and chat is the last place any of those belong: the operator reads
/// the category from telemetry, and the sender reads a sentence.
pub(crate) const FAILURE_REPLY: &str = "The agent could not complete this request.";
/// Fixed warning when capability work may have happened but the model produced no report.
pub(crate) const UNREPORTED_WORK_REPLY: &str = "The agent attempted capability work but could not report the result. Check the audit before retrying.";
/// Confirmation sent when Slack's authenticated Agent-session Stop event wins the completion race.
pub(crate) const STOPPED_REPLY: &str = "Stopped.";
/// One transport-independent normalization for a successful empty model answer.
pub(crate) const EMPTY_REPLY: &str = "[empty response]";

const SESSION_RUNNING: u8 = 0;
const SESSION_CANCELLED: u8 = 1;
const SESSION_COMPLETING: u8 = 2;

/// One conversation, for in-flight serialization only.
///
/// Deliberately subject-free, and deliberately not the history key. Two people talking at once in
/// one thread are one thing to serialize and two things to remember;
/// [`dekopon_harness::conversation::ConversationKey`] is the other question and carries the sender.
type AdmissionKey = (String, String, Option<String>);

/// One model client, shared by every session that routes to the same configured model.
pub(crate) type SharedModel = Arc<dyn ChatModel + Send + Sync>;

/// Builds the model client one route selected.
///
/// A seam rather than a direct call because the alternative is a test suite that cannot exercise
/// routing, admission, or authorization without a live model endpoint.
pub(crate) trait ModelFactory: Send + Sync {
    fn build(&self, model: &ModelConfig) -> Result<SharedModel, SessionError>;
}

/// The real factory: whatever `models:` configured, constructed exactly as `dekopon-run` does.
pub(crate) struct ConfiguredModels;

/// Builds the gateway's image generator once at startup, reading its credential only when a bound
/// route actually opts in, and before any transport begins accepting messages.
pub(crate) fn configured_image_generator(
    configured: Option<&ImageGeneratorConfig>,
    referenced: bool,
) -> Result<Option<Arc<dyn ImageGenerator>>, ImageGeneratorStartupError> {
    let Some(generator) = configured.filter(|_| referenced) else {
        return Ok(None);
    };
    let variable = generator.api_key_env.as_str();
    let credential = image_credential(variable, std::env::var_os(variable))?;
    let client = OpenAiImageGenerator::new(
        &generator.model,
        credential,
        std::time::Duration::from_millis(generator.timeout_ms),
    )?;
    Ok(Some(Arc::new(client) as Arc<dyn ImageGenerator>))
}

/// Resolves the image generator's credential, keeping which of the three problems it was.
///
/// Split from the environment read so the rule is reachable without a test mutating this process's
/// environment: `set_var` is unsafe in this edition and this workspace forbids unsafe outright.
pub(crate) fn image_credential(
    variable: &str,
    value: Option<std::ffi::OsString>,
) -> Result<String, ImageGeneratorStartupError> {
    credential_from(variable, value).map_err(|source| ImageGeneratorStartupError::Credential {
        variable: variable.to_owned(),
        source,
    })
}

/// The bearer token a configured model's `apiKeyEnv` names, when it names one.
///
/// Absent is not missing. A loopback llama.cpp needs no key and leaving the field out is how an
/// operator says so, which is why this answers `None` rather than refusing. Exported-but-blank is
/// missing: an empty bearer token is still sent as a header. Both used to read `env::var(..).ok()`
/// and become "no bearer token", so the gateway started clean and 401'd on the first message with
/// nothing anywhere naming the variable.
pub(crate) fn model_bearer_token(
    model: &ModelConfig,
) -> Result<Option<String>, ModelCredentialError> {
    let ModelConfig::OpenaiCompatible { api_key_env, .. } = model else {
        return Ok(None);
    };
    let Some(variable) = api_key_env.as_deref() else {
        return Ok(None);
    };
    model_credential(model.name(), variable, std::env::var_os(variable)).map(Some)
}

/// Split from the environment read for the same reason [`image_credential`] is.
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

impl ModelFactory for ConfiguredModels {
    fn build(&self, model: &ModelConfig) -> Result<SharedModel, SessionError> {
        match model {
            ModelConfig::OpenaiCompatible {
                endpoint,
                model,
                api_key_env,
                timeout_ms,
                ..
            } => {
                let bearer_token = api_key_env
                    .as_deref()
                    .map(|variable| model_credential(model, variable, std::env::var_os(variable)))
                    .transpose()?;
                Ok(Arc::new(OpenAiChatModel::new(
                    endpoint,
                    model,
                    bearer_token,
                    std::time::Duration::from_millis(*timeout_ms),
                )?))
            }
            ModelConfig::ChatgptSubscription {
                model,
                auth_file,
                timeout_ms,
                ..
            } => Ok(Arc::new(ChatGptCodexModel::new(
                model,
                auth_file.as_deref(),
                std::time::Duration::from_millis(*timeout_ms),
            )?)),
        }
    }
}

/// One client per configured model, built on first use and shared by every session after it.
///
/// A model client owns an HTTP agent and its connection pool, so rebuilding one per message paid a
/// fresh TCP and TLS handshake before the first token of every answer — on a Pi talking to a remote
/// endpoint, more added latency than the routing and authorization ahead of it cost together.
/// Everything that legitimately varies per message — the prompt cache key, the completion options —
/// is request-scoped and stays that way.
///
/// Keyed by the configured model name, which the loader has already proved unique, so two routes
/// naming one endpoint share its pool and two endpoints never share a client.
///
/// A build failure is not cached, because the two remaining ones are repairable without a restart:
/// a credential file an operator writes, and a model endpoint that was not listening. An `apiKeyEnv`
/// naming an unset or blank variable is not among them — this process cannot see a variable exported
/// after it started — so startup resolves every bound route's model credential before any transport
/// accepts work, and the daemon refuses to start rather than answering with a tokenless client.
pub(crate) struct ModelCache {
    factory: Arc<dyn ModelFactory>,
    clients: Mutex<HashMap<String, SharedModel>>,
}

impl ModelCache {
    pub(crate) fn new(factory: Arc<dyn ModelFactory>) -> Self {
        Self {
            factory,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The client for one configured model, building it if this is the first message to need it.
    pub(crate) fn client(&self, model: &ModelConfig) -> Result<SharedModel, SessionError> {
        if let Some(client) = self
            .clients
            .lock()
            .expect("gateway model clients")
            .get(model.name())
        {
            return Ok(Arc::clone(client));
        }
        // Built outside the lock: two sessions racing on the first message to one endpoint should
        // not serialize, and whichever finishes second discards its client rather than replacing a
        // pool another session is already using.
        let built = self.factory.build(model)?;
        Ok(Arc::clone(
            self.clients
                .lock()
                .expect("gateway model clients")
                .entry(model.name().to_owned())
                .or_insert(built),
        ))
    }
}

struct GatewayModelRegistry {
    cache: Arc<ModelCache>,
    models: Vec<Arc<ModelConfig>>,
}
impl ModelRegistry for GatewayModelRegistry {
    fn candidates(&self) -> Vec<dekopon_broker_protocol::ControlTarget> {
        self.models
            .iter()
            .map(|model| dekopon_broker_protocol::ControlTarget {
                model: model.name().parse().expect("validated configured model ID"),
                // Both built-in transports encode these options; remote acceptance is not promised.
                efforts: vec![
                    dekopon_core::Effort::ProviderDefault,
                    dekopon_core::Effort::Low,
                    dekopon_core::Effort::Medium,
                    dekopon_core::Effort::High,
                ],
            })
            .collect()
    }
    fn prepare(
        &self,
        selection: &dekopon_core::ModelSelection,
    ) -> Result<PreparedModel, PreparationError> {
        let configured = self
            .models
            .iter()
            .find(|m| m.name() == selection.model.as_str())
            .ok_or(PreparationError::UnknownModel)?;
        let client = self.cache.client(configured).map_err(|error| {
            tracing::warn!(
                cause_type = error.category(),
                "configured control client preparation failed"
            );
            PreparationError::Unavailable
        })?;
        let (backend, model) = match configured.as_ref() {
            ModelConfig::OpenaiCompatible { model, .. } => ("openai-compatible", model),
            ModelConfig::ChatgptSubscription { model, .. } => ("chatgpt-subscription", model),
        };
        Ok(PreparedModel {
            identity: ModelIdentity {
                configured: Some(selection.model.clone()),
                backend: backend.into(),
                model: model.clone(),
                effort: selection.effort,
            },
            client,
            accepts_images: configured.accepts_images(),
        })
    }
}

fn control_coordinate() -> String {
    IdSequence::new("control")
        .expect("constant bounded prefix")
        .trace()
        .as_str()
        .to_owned()
}

/// Admission control: a process-wide ceiling plus per-conversation serialization.
///
/// Two bounds because they answer different questions. The semaphore bounds what this daemon costs
/// at once; the in-flight set stops one conversation from queueing work on itself, which is what a
/// person does when a bot seems slow and they send the same thing again.
pub(crate) struct SessionGate {
    permits: Arc<Semaphore>,
    in_flight: Arc<Mutex<BTreeSet<AdmissionKey>>>,
}

impl SessionGate {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent)),
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Admits one session, or reports that this message must be refused.
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

/// Holds one session's permit and conversation slot until it is dropped.
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
    /// Fired exactly once, by the caller that won the race to cancel, into the broker leg's
    /// in-flight command-word run.
    handle: CancelHandle,
    signal: CancelSignal,
}

impl SessionCancellation {
    pub(crate) fn new() -> Self {
        let (handle, signal) = CancelSignal::pair();
        Self {
            state: Arc::new(AtomicU8::new(SESSION_RUNNING)),
            handle,
            signal,
        }
    }

    fn claim_completion(&self) -> bool {
        self.state
            .compare_exchange(
                SESSION_RUNNING,
                SESSION_COMPLETING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn cancel(&self) -> bool {
        let cancelled = self
            .state
            .compare_exchange(
                SESSION_RUNNING,
                SESSION_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        // Only the winner fires it, so a session that completed normally — whose drop guard
        // still calls this — never aborts a broker round trip it already finished.
        if cancelled {
            self.handle.cancel();
        }
        cancelled
    }

    /// The signal the broker leg supervises its command-word runs against.
    pub(crate) fn signal(&self) -> CancelSignal {
        self.signal.clone()
    }
}

impl CancellationProbe for SessionCancellation {
    fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == SESSION_CANCELLED
    }
}

/// Cancels synchronous work when its owning async session is aborted during shutdown.
struct CancellationOnDrop(SessionCancellation);

impl Drop for CancellationOnDrop {
    fn drop(&mut self) {
        let _ = self.0.cancel();
    }
}

/// Prevents a script from starting another capability call after a native Stop was observed.
///
/// A call already inside the delegate is not rollbackable; this check is the cooperative boundary
/// immediately before the broker proposal.
pub(crate) struct CancelAwareInvoker<I> {
    pub(crate) inner: I,
    pub(crate) cancellation: SessionCancellation,
}

impl<I: CapabilityInvoker> CapabilityInvoker for CancelAwareInvoker<I> {
    fn check_freshness(&self) -> Result<(), String> {
        self.inner.check_freshness()
    }

    fn granted(&self) -> Vec<String> {
        self.inner.granted()
    }

    fn is_granted(&self, capability: &str) -> bool {
        self.inner.is_granted(capability)
    }

    fn grants_namespace(&self, namespace: &str) -> bool {
        self.inner.grants_namespace(namespace)
    }

    fn command_words(&self) -> Vec<String> {
        self.inner.command_words()
    }

    // Forwarded rather than left to the default, which would answer this session's every command
    // word by materializing both legs' command-word lists first.
    fn has_command_word(&self, word: &str) -> bool {
        self.inner.has_command_word(word)
    }

    fn run_command(
        &self,
        word: &str,
        argv: &[String],
        stdin: Option<&str>,
    ) -> Option<dekopon_shell::CommandRun> {
        self.inner.run_command(word, argv, stdin)
    }

    fn describe(&self, capability: &str) -> Option<dekopon_shell::CapabilityDescription> {
        self.inner.describe(capability)
    }

    fn invoke(
        &self,
        capability: &str,
        input: serde_json::Value,
        secret_use: Option<dekopon_core::SecretUseProposal>,
    ) -> CapabilityCallResult {
        if self.cancellation.is_cancelled() {
            CapabilityCallResult::Denied {
                reason: "session-cancelled".to_owned(),
            }
        } else {
            self.inner.invoke(capability, input, secret_use)
        }
    }
}

type ActiveSessionKey = (String, String);

#[derive(Clone)]
struct ActiveSession {
    subject: dekopon_core::ExternalSubject,
    cancellation: SessionCancellation,
    activity: ActivityControl,
    replier: Arc<dyn ChatReplier>,
    reply: ReplyTarget,
}

/// A durable response to one native Stop event, sent outside the transport-reader task.
pub(crate) struct StopReply {
    pub replier: Arc<dyn ChatReplier>,
    pub target: ReplyTarget,
}

/// Active Agent sessions keyed only by authenticated transport-native conversation identity.
#[derive(Clone, Default)]
pub(crate) struct ActiveSessions {
    entries: Arc<Mutex<HashMap<ActiveSessionKey, ActiveSession>>>,
}

impl ActiveSessions {
    fn register(
        &self,
        message: &InboundMessage,
        cancellation: SessionCancellation,
        activity: ActivityControl,
        replier: Arc<dyn ChatReplier>,
    ) -> ActiveRegistration {
        let key = (message.transport.clone(), message.conversation_id.clone());
        let session = ActiveSession {
            subject: message.subject.clone(),
            cancellation: cancellation.clone(),
            activity,
            replier,
            reply: message.reply.clone(),
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
                // Admission should make this unreachable. Refusing to replace the owner keeps a
                // stale or malformed control event from cancelling a different generation.
                tracing::error!(event = "gateway_session_registry_conflict");
                false
            }
        };
        ActiveRegistration {
            entries: Arc::clone(&self.entries),
            key,
            cancellation,
            registered,
        }
    }

    pub(crate) fn stop(&self, request: &SessionStop) -> Option<StopReply> {
        let key = (request.transport.clone(), request.conversation_id.clone());
        let session = self
            .entries
            .lock()
            .expect("active session registry")
            .get(&key)
            .cloned()?;
        if session.subject != request.subject || !session.cancellation.cancel() {
            return None;
        }
        session.activity.finish();
        Some(StopReply {
            replier: session.replier,
            target: session.reply,
        })
    }
}

struct ActiveRegistration {
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
        }) {
            entries.remove(&self.key);
        }
    }
}

/// Everything shared by every session this daemon runs.
pub(crate) struct SessionRunner {
    pub broker: ResolvedBroker,
    pub models: Arc<ModelCache>,
    pub gate: SessionGate,
    pub reply_on_busy: bool,
    /// What `persistent` routes remember. Empty and untouched while every route is `oneShot`.
    pub conversations: BoundedConversationStore,
    /// The attachments live conversations carry, numbered so a model can ask for one.
    pub assets: Arc<AssetStore>,
    /// How each transport turns one of those references back into bytes, by transport name.
    pub asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>>,
    /// The gateway's model-credential image generator, built once at startup. A route reaches it
    /// only when it explicitly opts in.
    pub image_generator: Option<Arc<dyn ImageGenerator>>,
    /// Optional service-native in-flight activity, by transport name.
    pub activities: HashMap<String, Arc<dyn ChatActivity>>,
    /// Bounded transport-owned Slack Agent thread claims, by transport name.
    pub thread_ownership: HashMap<String, Arc<dyn ThreadOwnership>>,
    /// Native Agent sessions that can receive authenticated Stop events.
    pub active_sessions: ActiveSessions,
    /// Best-effort informational usage deltas for the broker-hosted web UI.
    pub usage_reports: Option<mpsc::Sender<ModelUsageReport>>,
}

/// Runs one routed message end to end, answering unless an optional continuation declines.
pub(crate) async fn run_session(
    runner: Arc<SessionRunner>,
    route: BoundRoute,
    message: InboundMessage,
    replier: Arc<dyn ChatReplier>,
) {
    let span = tracing::info_span!(
        "gateway.message",
        transport = %message.transport,
        agent = %route.agent,
        outcome = tracing::field::Empty
    );
    let outcome = execute(runner, route, message, replier)
        .instrument(span.clone())
        .await;
    span.record("outcome", outcome);
}

async fn execute(
    runner: Arc<SessionRunner>,
    route: BoundRoute,
    message: InboundMessage,
    replier: Arc<dyn ChatReplier>,
) -> &'static str {
    // Canonical subject identifiers and chat text are payload telemetry, never metadata. The
    // default `gateway.message` span carries transport, agent, and outcome and nothing that
    // identifies a person or repeats what they said.
    if dekopon_core::telemetry_payloads() {
        tracing::info!(
            target: "dekopond::audit",
            {
                audit.event = "gateway.message.received",
                subject = %message.subject,
                channel = message.channel.as_str(),
                text = message.text.as_str(),
            },
            "gateway message received"
        );
    }

    let key = (
        message.transport.clone(),
        message.channel.clone(),
        message.thread.clone(),
    );
    let Some(admission) = runner.gate.admit(key) else {
        tracing::info!(event = "gateway_session_rejected", reason = "busy");
        if runner.reply_on_busy {
            answer(&replier, &message, BUSY_REPLY).await;
        }
        return "busy";
    };

    // Both conversation fields are zero on a `oneShot` route and on the first message of any
    // conversation, which makes "was this session seeded" a filter rather than a guess. They are a
    // count and a byte total: the history itself is chat text and stays behind the payload gate.
    let outcome = session(&runner, &route, &message, &replier)
        .instrument(tracing::info_span!(
            "gateway.session",
            agent = %route.agent,
            conversation.turns = tracing::field::Empty,
            conversation.bytes = tracing::field::Empty,
        ))
        .await;
    drop(admission);
    outcome
}

async fn session(
    runner: &SessionRunner,
    route: &BoundRoute,
    message: &InboundMessage,
    replier: &Arc<dyn ChatReplier>,
) -> &'static str {
    let key = ConversationKey::scoped(
        route.agent.as_str(),
        &route.cache_key,
        &message.transport,
        &message.channel,
        &message.conversation_id,
        &message.subject.canonical(),
    );
    let leg = match connect(runner, route, message).await {
        Ok(leg) => leg,
        // A refused attestation never reaches a decision record, so it arrives as a transport-level
        // code rather than an empty capability set. It is still the broker saying no, and telling
        // the sender "something broke" would send them to an operator over a working refusal.
        Err(SessionError::BrokerLeg(BrokerLegError::Client(ClientError::Remote {
            code, ..
        }))) if code == ERROR_UNAUTHENTICATED => {
            revoke_thread_ownership(runner, message);
            runner
                .conversations
                .remove(&key, EvictionReason::GrantChanged);
            tracing::info!(
                event = "gateway_session_rejected",
                reason = "attestation-refused"
            );
            answer(replier, message, UNAUTHORIZED_REPLY).await;
            return "unauthorized";
        }
        Err(error) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category()
            );
            answer(replier, message, FAILURE_REPLY).await;
            return "failed";
        }
    };
    // Never cached and never remembered as a permission: this is a fresh answer from the broker
    // about what this subject may reach through this agent, on this message.
    let granted = leg.granted();

    // The authorization gate, and it costs nothing: an empty answer is a complete answer, so there
    // is no model call to make. Removing the entry rather than only refusing is the other half —
    // a revoked subject whose exchange stayed resident for the rest of its idle timeout would be
    // holding exactly the text the revocation was about.
    if granted.is_empty() {
        revoke_thread_ownership(runner, message);
        runner
            .conversations
            .remove(&key, EvictionReason::GrantChanged);
        tracing::info!(event = "gateway_session_rejected", reason = "unauthorized");
        answer(replier, message, UNAUTHORIZED_REPLY).await;
        return "unauthorized";
    }
    // An Agent thread becomes a continuation surface only after this exact sender's fresh broker
    // grant succeeded. Merely mentioning the bot, or putting coordinates in model text, cannot
    // claim one.
    claim_thread_ownership(runner, message);
    let agent_config = agent_config_view(
        route.agent.as_str(),
        &route.description,
        route.model_class.as_deref(),
        route.instructions.as_deref(),
        &route.skills,
        route.limits,
        route.conversation,
        &leg,
    );

    // The lookup happens *after* the authorization gate because the grant comparison needs a fresh
    // grant to compare against. `Instant` is supplied by the caller rather than read inside the
    // store so eviction has a clock a test can drive.
    // Built once for this message and handed to the engine below. It is the same bounded
    // projection the session's runtime would build for itself; building it twice per message
    // re-read and re-encoded every granted schema for a fingerprint we already have.
    let capabilities = match dekopon_harness::bootstrap::CapabilitySnapshot::from_invoker(&leg) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::error!(event = "gateway_session_failed", category = "invalid-bootstrap", cause = %error);
            answer(replier, message, FAILURE_REPLY).await;
            return "failed";
        }
    };
    let surface = vec![capabilities.fingerprint(), leg.surface_epoch().to_string()];
    let checkpoint_scope = key.commitment();
    let window = route.conversation.window();
    let ConversationSeed {
        history: seeded,
        cache_key,
    } = match window {
        Some(window) => runner
            .conversations
            .begin(&key, &surface, window, Instant::now()),
        // A route that remembers nothing has no conversation to name, so its messages route to the
        // route's own lane: the instructions and tools ahead of every one of them are the only
        // prefix they share, and they share all of it. `routes::BoundRoute::cache_key` has the
        // argument for why that is not a sender leak.
        None => ConversationSeed {
            history: History::default(),
            cache_key: route.cache_key.clone(),
        },
    };
    let span = tracing::Span::current();
    span.record("conversation.turns", seeded.len());
    span.record("conversation.bytes", seeded.bytes());
    // The key names nobody, but it still joins one person's turns to each other, which is the
    // linkage the metadata-only default exists to withhold. It rides the payload gate for that
    // reason, and on a line of its own: the canonical subject is on `gateway.message.received`, and
    // the two must never meet in one record.
    if dekopon_core::telemetry_payloads() {
        tracing::info!(
            target: "dekopond::audit",
            {
                audit.event = "gateway.session.cache_key",
                prompt.cache_key = cache_key.as_str(),
                conversation.persistent = window.is_some(),
            },
            "gateway session prompt cache key"
        );
    }

    let memory_surface = leg.chat_memory_surface().cloned();
    let surface_epoch = leg.surface_epoch().clone();
    let chat_claim = chat_claim(route, message).ok();
    let image_generator = match (route.image_generator, runner.image_generator.as_ref()) {
        (false, _) => None,
        (true, Some(generator)) => Some(Arc::clone(generator)),
        (true, None) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = "image-generator-unavailable"
            );
            answer(replier, message, FAILURE_REPLY).await;
            return "failed";
        }
    };
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
    // Numbered here rather than in the transport: the identifier belongs to the store, and two
    // transports minting their own would collide inside one conversation.
    let images_supported = route.model.accepts_images();
    let registered = runner.assets.assets_for(
        &message.conversation_id,
        message.assets.clone(),
        images_supported,
        Instant::now(),
    );
    // Bounded again after the note is appended, because the invariant is on the whole prompt the
    // model reads rather than on the half of it the sender wrote.
    let text = match asset::reference_note(&registered, images_supported) {
        Some(note) if message.text.trim().is_empty() => note,
        Some(note) => bound_inbound(&format!("{}\n\n{note}", message.text)),
        None => message.text.clone(),
    };
    let assets = SessionAssets::new(
        Arc::clone(&runner.assets),
        message.conversation_id.clone(),
        runner.asset_fetchers.get(&message.transport).cloned(),
        tokio::runtime::Handle::current(),
        images_supported,
        registered.fetchable,
    );
    let shell = ShellLimits {
        max_capability_calls: limits.max_capability_calls,
        ..ShellLimits::default()
    };
    // Request-scoped and built here rather than handed to `ModelFactory::build`, which is what lets
    // `ModelCache` share one client across sessions: a key captured in a constructor would describe
    // the first conversation forever while quietly mislabeling every later one.
    let options = CompletionOptions::default()
        .with_prompt_cache_key(cache_key.clone())
        .with_effort(model_config.effort());
    let configured_controls = route.controls.clone();
    let control_executor = tokio::runtime::Handle::current();
    let control_client = if configured_controls.is_some() {
        let make_client = || -> Result<_, SessionError> {
            let client = BrokerClient::new(
                &runner.broker.socket_path,
                runner.broker.server_uid,
                runner.broker.frame,
            )?;
            Ok(client.control_client(
                dekopon_broker_protocol::ControlScope {
                    agent: route.agent.clone(),
                    job: control_coordinate()
                        .parse()
                        .expect("bounded opaque coordinate"),
                    session: control_coordinate()
                        .parse()
                        .expect("bounded opaque coordinate"),
                    request: control_coordinate()
                        .parse()
                        .expect("bounded opaque coordinate"),
                    generation: control_coordinate()
                        .parse()
                        .expect("bounded opaque coordinate"),
                },
                surface_epoch.clone(),
                Some(self::chat_claim(route, message)?),
                0,
            )?)
        };
        match make_client() {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::error!(
                    event = "gateway_session_failed",
                    category = error.category()
                );
                answer(replier, message, FAILURE_REPLY).await;
                return "failed";
            }
        }
    } else {
        None
    };

    // Activity is armed only after the fresh authorization gate and immediately before the costly
    // model/tool work. The registry and cancellation probe share one generation, so a native Slack
    // Stop event can win exactly once against the terminal answer.
    let driver = runner.activities.get(&message.transport).cloned();
    let activity_enabled = driver.is_some() && message.activity.is_some();
    let cancellation = SessionCancellation::new();
    // A Stop that wins the race also aborts whichever broker command-word run the script is
    // parked on, so the blocking loop reaches its next cancellation check instead of waiting out
    // a broker that is still working.
    let leg = leg.with_cancel_signal(cancellation.signal());
    let reply_optional = message
        .thread_continuation
        .as_ref()
        .is_some_and(|c| c.inherited);
    let mut activity = ActivityLease::start(driver, message.activity.clone(), reply_optional);
    let activity_publisher = activity.publisher();
    let activity_labels = route.activity_labels.clone();
    let _active_registration = activity_enabled.then(|| {
        runner.active_sessions.register(
            message,
            cancellation.clone(),
            activity.control(),
            Arc::clone(replier),
        )
    });
    // Declared last so task abortion drops this guard first, marking the blocking loop cancelled
    // before activity/registry cleanup releases the rest of the async session state.
    let _cancel_on_drop = CancellationOnDrop(cancellation.clone());

    // The prompt loop and the interpreter are both synchronous and both can block for a long time
    // — a model round trip, a script that sleeps, a broker call per command. Running that on a
    // runtime worker would stall every other session in the process.
    let blocking_span = span.clone();
    let usage = Arc::new(dekopon_harness::accounting::JobAccounting::default());
    let observed_usage = Arc::clone(&usage);
    let prompt_cancellation = cancellation.clone();
    let reply_optional = message
        .thread_continuation
        .as_ref()
        .is_some_and(|continuation| continuation.inherited);
    // Shared with the route rather than cloned: the skill text is read once at startup.
    let skills = Arc::clone(&route.skills);
    let improvement_suggestions = route.improvement_suggestions;
    let result = tokio::task::spawn_blocking(move || {
        let _entered = blocking_span.enter();
        // Resolved before the accumulator exists, so a model client that cannot be constructed
        // returns without a turn: nothing was asked, so there is no exchange to remember. Only the
        // first message to reach a given endpoint actually builds one.
        let model = match models.client(&model_config) {
            Ok(model) => model,
            Err(error) => return (Err(error), None, None),
        };
        let registry = GatewayModelRegistry {
            cache: Arc::clone(&models),
            models: configured_controls.as_ref().map_or_else(Vec::new, |c| c.models.clone()),
        };
        let controls = match control_client.map(|client| SessionControls::new(
            &registry, dekopon_core::ModelSelection {
                model: model_config.name().parse().expect("validated configured model ID"), effort: model_config.effort(),
            }, client, control_executor, configured_controls.as_ref().expect("enabled controls").max_attempts,
        )).transpose() {
            Ok(controls) => controls,
            Err(error) => return (Err(SessionError::Prompt(error.into())), None, None),
        };
        let runtime = ShellRuntime {
            invoker: CancelAwareInvoker {
                inner: leg,
                cancellation: prompt_cancellation.clone(),
            },
            limits: shell,
            curl_capability: None,
        };
        // `history` is the accumulator rather than a return value, so this session's exchange is
        // recorded into it whichever way the loop ends.
        let mut history = seeded;
        let generated_image = GeneratedImageOutput::default();
        let mut inputs = SessionBootstrap::new(
            &text,
            limits,
            match model_config.as_ref() {
                ModelConfig::OpenaiCompatible { model, .. }
                | ModelConfig::ChatgptSubscription { model, .. } => model,
            },
        )
        .with_surface_epoch(&surface_epoch)
                .with_scope(&checkpoint_scope)
        .with_capability_snapshot(&capabilities)
        .with_system(instructions.as_deref())
        .with_skills(&skills)
        .with_options(&options)
        .with_assets(&assets)
        .with_accounting(observed_usage.as_ref())
        .with_model_identity(ModelIdentity { configured: Some(model_config.name().parse().expect("configured model")), backend: model.model_identity().0.to_owned(), model: match model_config.as_ref() { ModelConfig::OpenaiCompatible { model, .. } | ModelConfig::ChatgptSubscription { model, .. } => model.clone() }, effort: model_config.effort() })
        .with_agent_config(&agent_config)
        .with_cancellation(&prompt_cancellation);
        if let Some(publisher) = &activity_publisher { inputs = inputs.with_activity(publisher, &activity_labels); }
        if let Some(controls) = &controls { inputs = inputs.with_controls(controls); }
        if let Some(generator) = image_generator.as_deref() {
            inputs = inputs.with_image_generation(generator, &generated_image);
        }
        if improvement_suggestions {
            inputs = inputs.with_improvement_suggestions();
        }
        if reply_optional {
            inputs = inputs.with_optional_reply();
        }
        let prior_job = history.turns().last().map(|r| r.job.clone());
        let outcome = SessionEngine::new(model.as_ref(), &runtime)
            .run(inputs, &mut history)
            .map_err(SessionError::from);
        // The independent checkpoint owns every started job, even when bounded history evicts
        // its text. Unknown effects and Stop must still reach the scoped store and finalizer.
        let turn = match &outcome {
            Err(SessionError::Prompt(PromptError::Interrupted { checkpoint, .. })) => Some(checkpoint.record.clone()),
            _ => {
                let job = observed_usage.snapshot().job;
                if job.is_empty() { None } else {
                    match dekopon_harness::checkpoint::memory_checkpoints().load(&job) {
                        Ok(saved) => Some(saved.record),
                        Err(error) => {
                            tracing::error!(event = "gateway_session_failed", category = "checkpoint-load", cause = %error);
                            history.turns().last().filter(|r| Some(&r.job) != prior_job.as_ref()).cloned()
                        }
                    }
                }
            }
        };
        let image = outcome.is_ok().then(|| generated_image.take()).flatten();
        (outcome, turn, image)
    })
    .await;

    if let Some(report) = usage.take_report()
        && let Some(reports) = &runner.usage_reports
        && reports.try_send(report).is_err()
    {
        // Informational accounting must never delay or fail a paid-for answer. A bounded full or
        // closed queue loses a live dashboard delta and leaves OTLP accounting unchanged.
        tracing::warn!(event = "gateway_usage_report_dropped");
    }

    let (outcome, turn, generated_image) = match result {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(event = "gateway_session_failed", category = "session-task", cause = %error);
            if !cancellation.claim_completion() {
                tracing::info!(event = "gateway_session_cancelled");
                activity.finish_in_background();
                return "cancelled";
            }
            // The task itself died, so there is no history to trust and nothing to record.
            activity.seal();
            let replied = answer(replier, message, FAILURE_REPLY).await;
            activity.finish_in_background();
            return if replied { "failed" } else { "reply-failed" };
        }
    };

    let remember = |turn: Option<JobRecord>, delivery: DeliveryDisposition| {
        let job = usage.snapshot().job;
        if !job.is_empty()
            && let Err(error) = dekopon_harness::checkpoint::finalize_delivery(
                &job,
                delivery.clone(),
                usage.as_ref(),
            )
        {
            tracing::error!(event = "gateway_session_failed", category = "checkpoint-finalization", cause = %error);
        }
        if let Some(mut turn) = turn {
            turn.delivery = delivery;
            if let Some(window) = window
                && let Err(error) = runner.conversations.commit(
                    &key,
                    &surface,
                    window,
                    turn,
                    &cache_key,
                    Instant::now(),
                )
            {
                tracing::warn!(event = "gateway_conversation_append_refused", cause = %error);
            }
        }
    };

    if matches!(&outcome, Err(SessionError::Prompt(PromptError::Cancelled)))
        || cancellation.is_cancelled()
        || !cancellation.claim_completion()
    {
        remember(turn, DeliveryDisposition::Cancelled);
        tracing::info!(event = "gateway_session_cancelled");
        activity.finish_in_background();
        return "cancelled";
    }

    // Seal renewal before terminal delivery or deliberate silence, but do no remote cleanup on
    // this latency-sensitive path. Slack's explicit `active` and reaction removal run only after
    // the completion decision is durable in gateway state.
    activity.seal();

    if matches!(
        &outcome,
        Ok(outcome) if outcome.disposition == ReplyDisposition::Suppress
    ) {
        // No reply call means no acceptance receipt and therefore no durable recording. Native
        // activity still returns to its inactive state through the separate cosmetic surface. The
        // unanswered in-process turn was committed above so a later continuation still sees what
        // the person said. Activity cleanup remains best effort and cannot create a chat message.
        remember(turn, DeliveryDisposition::Suppressed);
        activity.finish_in_background();
        return "declined";
    }

    let (answer_text, completed_outcome, recordable) = match outcome {
        Ok(outcome) if outcome.answer.is_empty() => (EMPTY_REPLY.to_owned(), "answered", true),
        Ok(outcome) => (outcome.answer, "answered", true),
        Err(SessionError::Prompt(PromptError::UnreportedCapabilityWork)) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = "unreported-capability-work"
            );
            (UNREPORTED_WORK_REPLY.to_owned(), "failed", false)
        }
        Err(error) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category()
            );
            (FAILURE_REPLY.to_owned(), "failed", false)
        }
    };
    let delivered_answer = bound_outbound(&answer_text);
    let reply = match generated_image {
        Some(image) => OutboundReply::with_image(delivered_answer.clone(), image),
        None => OutboundReply::text(delivered_answer.clone()),
    };
    let delivery = deliver(replier, message, reply).await;
    activity.finish_in_background();
    match delivery {
        Ok(receipt) if receipt.accepted() => {
            remember(
                turn,
                DeliveryDisposition::Accepted {
                    text: delivered_answer.clone(),
                },
            );
            if recordable
                && memory_surface.is_some()
                && let Some(claim) = chat_claim
            {
                record_delivered_turn(runner, message, claim, delivered_answer).await;
            }
            completed_outcome
        }
        Ok(_) | Err(TransportError::PartialDelivery) => {
            remember(turn, DeliveryDisposition::Partial);
            "reply-failed"
        }
        Err(error) => {
            let disposition = if matches!(&error, TransportError::Service { code } if matches!(code.as_str(), "ratelimited" | "post-capacity" | "invalid_auth" | "token_revoked" | "missing_scope"))
            {
                DeliveryDisposition::Failed
            } else {
                DeliveryDisposition::Unknown
            };
            remember(turn, disposition);
            "reply-failed"
        }
    }
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

/// Builds the credential-free meta view from gateway-owned catalog fields and the broker's fresh
/// subject-specific capability snapshot.
///
/// Deliberately takes no [`ModelConfig`], broker configuration, transport message, subject, or
/// principal. Those are exactly the places credentials, endpoints, paths, and identity live, and a
/// constructor that cannot receive them is stronger than one expected to remember to redact them.
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
    limits: dekopon_harness::session::PromptLimits,
    conversation: ConversationPolicy,
    leg: &BrokerLeg,
) -> AgentConfigView {
    let conversation = match conversation {
        ConversationPolicy::OneShot => ConversationConfigView::OneShot,
        ConversationPolicy::Persistent(window) => ConversationConfigView::Persistent {
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
            conversation,
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

/// Opens this session's attested broker leg.
///
/// A fresh client per session rather than a shared one, because the protocol client connects per
/// call anyway: there is no connection to reuse, and a per-session client keeps one session's
/// identifiers and attestation entirely its own.
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
    BrokerLeg::connect(client, TRACE_PREFIX, Some(chat_claim(route, message)?))
        .await
        .map_err(SessionError::from)
}

fn chat_claim(route: &BoundRoute, message: &InboundMessage) -> Result<Attestation, SessionError> {
    let transport = message
        .transport
        .parse()
        .map_err(SessionError::TransportId)?;
    let (channel, conversation) = match message.transport_kind {
        dekopon_broker_protocol::ChatTransportKind::Slack => (
            message.channel.to_ascii_lowercase(),
            message.conversation_id.to_ascii_lowercase(),
        ),
        _ => (message.channel.clone(), message.conversation_id.clone()),
    };
    Ok(Attestation::for_chat(
        message.subject.clone(),
        route.agent.clone(),
        ChatScopeClaim {
            transport,
            kind: message.transport_kind,
            channel,
            conversation,
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
        let identifiers = IdSequence::new("dekopond-memory-record").map_err(|error| {
            MemoryRecordFailure::Broker(BrokerLegError::SessionIdentifier(error))
        })?;
        let id = identifiers.next_invocation().map_err(|error| {
            MemoryRecordFailure::Broker(BrokerLegError::SessionIdentifier(error))
        })?;
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
                    id,
                    trace: identifiers.trace().clone(),
                    trace_parent: current_trace_parent(),
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
    match message.transport_kind {
        dekopon_broker_protocol::ChatTransportKind::Slack => Some(DeliveryIdentity::Slack {
            channel: scope.channel.clone(),
            timestamp: message.message_id.clone(),
        }),
        dekopon_broker_protocol::ChatTransportKind::Discord => Some(DeliveryIdentity::Discord {
            channel: scope.channel.clone(),
            message: message.message_id.clone(),
        }),
        dekopon_broker_protocol::ChatTransportKind::Telegram => {
            let topic = scope
                .conversation
                .strip_prefix(&format!("{}:topic:", scope.channel))
                .map(str::to_owned);
            Some(DeliveryIdentity::Telegram {
                chat: scope.channel.clone(),
                topic,
                message: message.message_id.clone(),
            })
        }
        dekopon_broker_protocol::ChatTransportKind::Whatsapp => {
            let mut parts = scope.channel.split(':');
            let waba = parts.next()?.to_owned();
            let phone_number = parts.next()?.to_owned();
            let _sender = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            Some(DeliveryIdentity::Whatsapp {
                waba,
                phone_number,
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
                conversation: scope.conversation.clone(),
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
            // Never copy a future provider/public error into telemetry. The broker result is
            // bounded, but an allowlist keeps this category stable and content-free by proof.
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
        MemoryRecordFailure::Broker(BrokerLegError::Bootstrap(_)) => "invalid-bootstrap",
        MemoryRecordFailure::Broker(BrokerLegError::Client(_)) => "broker",
        MemoryRecordFailure::Broker(BrokerLegError::SessionIdentifier(_)) => "identifier",
        MemoryRecordFailure::Broker(BrokerLegError::DuplicateCapabilities { .. }) => {
            "duplicate-capability"
        }
        MemoryRecordFailure::Outcome(category) => category,
    }
}

/// Sends one answer, reporting whether it arrived.
///
/// The outbound bound is applied here, once, rather than in each transport: a model writes this
/// text, every chat service rejects or mangles an oversized post, and one bound at the session
/// boundary is one place to read rather than three places to keep in agreement.
async fn answer(replier: &Arc<dyn ChatReplier>, message: &InboundMessage, text: &str) -> bool {
    deliver(replier, message, OutboundReply::text(bound_outbound(text)))
        .await
        .is_ok_and(|receipt| receipt.accepted())
}

async fn deliver(
    replier: &Arc<dyn ChatReplier>,
    message: &InboundMessage,
    reply: OutboundReply,
) -> Result<DeliveryReceipt, TransportError> {
    match replier.reply(message.reply.clone(), reply).await {
        Ok(receipt) => Ok(receipt),
        Err(error) => {
            tracing::error!(event = "gateway_reply_failed", category = error.category());
            Err(error)
        }
    }
}

/// Startup failure while resolving the configured image generator.
#[derive(Debug, Error)]
pub enum ImageGeneratorStartupError {
    /// The named variable is unset, blank, or not UTF-8; the source says which.
    #[error("image generator credential environment variable {variable} is unusable")]
    Credential {
        /// Owner-authored variable name, never its value.
        variable: String,
        #[source]
        source: TransportError,
    },
    #[error("image generator client configuration is invalid")]
    Client(#[from] ImageGenerationError),
}

/// The credential one configured model names could not be resolved.
///
/// Its own type rather than a [`SessionError`] variant alone, because startup resolves it before
/// any transport accepts work and a session resolves it again when it builds the client. One
/// definition, two readers: a pre-flight that could accept what the enforcing path rejects would be
/// worse than no pre-flight.
#[derive(Debug, Error)]
#[error("model {model:?} credential environment variable {variable} is unusable")]
pub struct ModelCredentialError {
    /// Owner-authored model name.
    pub model: String,
    /// Owner-authored variable name, never its value.
    pub variable: String,
    #[source]
    source: TransportError,
}

/// A session that could not run to completion.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("broker client could not be created")]
    BrokerClient(#[from] ClientError),
    #[error("broker leg could not be opened")]
    BrokerLeg(#[from] BrokerLegError),
    #[error("configured chat transport identifier is invalid")]
    TransportId(#[source] dekopon_core::IdentifierError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    ModelCredential(#[from] ModelCredentialError),
    #[error(transparent)]
    ChatGpt(#[from] dekopon_model::chatgpt::ChatGptError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
}

impl SessionError {
    /// Stable low-cardinality category, never the underlying message.
    ///
    /// Several variants wrap untrusted model, provider, or transport text, and `docs/observability.md`
    /// keeps that out of exported telemetry. An operator correlates this with the daemon's own logs.
    pub fn category(&self) -> &'static str {
        match self {
            Self::BrokerClient(_) => "broker-client",
            Self::BrokerLeg(_) => "broker-leg",
            Self::TransportId(_) => "transport-id",
            Self::Model(_) => "model",
            Self::ModelCredential(_) => "model-credential",
            Self::ChatGpt(_) => "chatgpt",
            Self::Prompt(error) => error.telemetry_kind(),
        }
    }
}
