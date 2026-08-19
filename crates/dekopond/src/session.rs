//! One routed message, from admission through the answer that goes back to chat.
//!
//! A session holds no authority. It opens an *attested* broker leg naming the sender, and whatever
//! that leg reports as granted is what the broker decided the sender may reach through this agent.
//! An empty grant ends the session before a single model token is spent, which is deliberate: the
//! cheapest possible refusal, and one that cannot be talked out of by the message text.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Instant,
};

use dekopon_agent::{
    BrokerLeg, BrokerLegError, ShellRuntime,
    meta::{AgentConfigView, ConversationConfigView, SessionConfigView},
    prompt::{History, ModelUsageObserver, PromptError, SessionInputs, run_prompt_session},
};
use dekopon_broker_protocol::{BrokerClient, ClientError, ERROR_UNAUTHENTICATED, ModelUsageReport};
use dekopon_model::{
    chatgpt::ChatGptCodexModel,
    model::{ChatModel, CompletionOptions, ModelError, ModelUsage, OpenAiChatModel},
};
use dekopon_shell::{CapabilityInvoker as _, Limits as ShellLimits};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tracing::Instrument as _;

use crate::{
    asset::{self, AssetStore, SessionAssets},
    config::{ConversationPolicy, ModelConfig, ResolvedBroker},
    conversation::{ConversationKey, ConversationSeed, ConversationStore, EvictionReason},
    routes::BoundRoute,
    transport::{AssetFetcher, ChatReplier, InboundMessage, bound_inbound, bound_outbound},
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

/// One conversation, for in-flight serialization only.
///
/// Deliberately subject-free, and deliberately not the history key. Two people talking at once in
/// one thread are one thing to serialize and two things to remember; `ConversationKey` in
/// [`crate::conversation`] is the other question and carries the sender.
type AdmissionKey = (String, String, Option<String>);

/// Builds the model client one route selected.
///
/// A seam rather than a direct call because the alternative is a test suite that cannot exercise
/// routing, admission, or authorization without a live model endpoint.
pub(crate) trait ModelFactory: Send + Sync {
    fn build(&self, model: &ModelConfig) -> Result<Box<dyn ChatModel + Send>, SessionError>;
}

/// The real factory: whatever `models:` configured, constructed exactly as `dekopon-run` does.
pub(crate) struct ConfiguredModels;

impl ModelFactory for ConfiguredModels {
    fn build(&self, model: &ModelConfig) -> Result<Box<dyn ChatModel + Send>, SessionError> {
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
                    .and_then(|name| std::env::var(name).ok());
                Ok(Box::new(OpenAiChatModel::new(
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
            } => Ok(Box::new(ChatGptCodexModel::new(
                model,
                auth_file.as_deref(),
                std::time::Duration::from_millis(*timeout_ms),
            )?)),
        }
    }
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

/// Everything shared by every session this daemon runs.
pub(crate) struct SessionRunner {
    pub broker: ResolvedBroker,
    pub models: Arc<dyn ModelFactory>,
    pub gate: SessionGate,
    pub reply_on_busy: bool,
    /// What `persistent` routes remember. Empty and untouched while every route is `oneShot`.
    pub conversations: ConversationStore,
    /// The attachments live conversations carry, numbered so a model can ask for one.
    pub assets: Arc<AssetStore>,
    /// How each transport turns one of those references back into bytes, by transport name.
    pub asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>>,
    /// Best-effort informational usage deltas for the broker-hosted web UI.
    pub usage_reports: Option<mpsc::Sender<ModelUsageReport>>,
}

#[derive(Default)]
struct UsageAccumulator(Mutex<ModelUsageReport>);

impl UsageAccumulator {
    fn report(&self) -> ModelUsageReport {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ModelUsageObserver for UsageAccumulator {
    fn observe(&self, usage: Option<ModelUsage>) {
        let mut report = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        report.model_calls = report.model_calls.saturating_add(1);
        let usage = usage.unwrap_or_default();
        (report.input_tokens, report.input_unreported_calls) = accumulated(
            report.input_tokens,
            report.input_unreported_calls,
            usage.input_tokens,
        );
        (
            report.cached_input_tokens,
            report.cached_input_unreported_calls,
        ) = accumulated(
            report.cached_input_tokens,
            report.cached_input_unreported_calls,
            usage.cached_input_tokens,
        );
        (report.output_tokens, report.output_unreported_calls) = accumulated(
            report.output_tokens,
            report.output_unreported_calls,
            usage.output_tokens,
        );
        (
            report.reasoning_output_tokens,
            report.reasoning_unreported_calls,
        ) = accumulated(
            report.reasoning_output_tokens,
            report.reasoning_unreported_calls,
            usage.reasoning_output_tokens,
        );
        (report.total_tokens, report.total_unreported_calls) = accumulated(
            report.total_tokens,
            report.total_unreported_calls,
            usage.total_tokens,
        );
    }
}

fn accumulated(total: u64, unreported: u64, value: Option<u64>) -> (u64, u64) {
    match value {
        Some(value) => (total.saturating_add(value), unreported),
        None => (total, unreported.saturating_add(1)),
    }
}

/// Runs one routed message end to end, answering in chat whatever happens.
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
    let leg = match connect(runner, route, message).await {
        Ok(leg) => leg,
        // A refused attestation never reaches a decision record, so it arrives as a transport-level
        // code rather than an empty capability set. It is still the broker saying no, and telling
        // the sender "something broke" would send them to an operator over a working refusal.
        Err(SessionError::BrokerLeg(BrokerLegError::Client(ClientError::Remote {
            code, ..
        }))) if code == ERROR_UNAUTHENTICATED => {
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
    let key = ConversationKey::new(
        &message.transport,
        &message.conversation_id,
        &message.subject.canonical(),
    );
    // The authorization gate, and it costs nothing: an empty answer is a complete answer, so there
    // is no model call to make. Removing the entry rather than only refusing is the other half —
    // a revoked subject whose exchange stayed resident for the rest of its idle timeout would be
    // holding exactly the text the revocation was about.
    if granted.is_empty() {
        runner
            .conversations
            .remove(&key, EvictionReason::GrantChanged);
        tracing::info!(event = "gateway_session_rejected", reason = "unauthorized");
        answer(replier, message, UNAUTHORIZED_REPLY).await;
        return "unauthorized";
    }
    let agent_config = agent_config_view(
        route.agent.as_str(),
        &route.description,
        route.model_class.as_deref(),
        route.instructions.as_deref(),
        route.limits,
        route.conversation,
        &leg,
    );

    // The lookup happens *after* the authorization gate because the grant comparison needs a fresh
    // grant to compare against. `Instant` is supplied by the caller rather than read inside the
    // store so eviction has a clock a test can drive.
    let window = route.conversation.window();
    let ConversationSeed {
        history: seeded,
        cache_key,
    } = match window {
        Some(window) => runner
            .conversations
            .begin(&key, &granted, window, Instant::now()),
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

    let model_config = Arc::clone(&route.model);
    let models = Arc::clone(&runner.models);
    let limits = route.limits;
    let instructions = route.instructions.clone();
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
    // Request-scoped and built here rather than handed to `ModelFactory::build`. The client is
    // rebuilt per message today and the obvious optimization is to share one across sessions, at
    // which point a key captured in a constructor would describe the first conversation forever
    // while quietly mislabeling every later one.
    let options = CompletionOptions::default().with_prompt_cache_key(cache_key.clone());
    // The prompt loop and the interpreter are both synchronous and both can block for a long time
    // — a model round trip, a script that sleeps, a broker call per command. Running that on a
    // runtime worker would stall every other session in the process.
    let blocking_span = span.clone();
    let usage = Arc::new(UsageAccumulator::default());
    let observed_usage = Arc::clone(&usage);
    let result = tokio::task::spawn_blocking(move || {
        let _entered = blocking_span.enter();
        // Built before the accumulator exists, so a model client that cannot be constructed
        // returns without a turn: nothing was asked, so there is no exchange to remember.
        let model = match models.build(&model_config) {
            Ok(model) => model,
            Err(error) => return (Err(error), None),
        };
        let runtime = ShellRuntime {
            invoker: leg,
            limits: shell,
            curl_capability: None,
        };
        // `history` is the accumulator rather than a return value, so this session's exchange is
        // recorded into it whichever way the loop ends.
        let mut history = seeded;
        let outcome = run_prompt_session(
            model.as_ref(),
            &runtime,
            SessionInputs::new(&text, limits)
                .with_system(instructions.as_deref())
                .with_options(&options)
                .with_assets(&assets)
                .with_usage_observer(observed_usage.as_ref())
                .with_agent_config(&agent_config),
            &mut history,
        )
        .map_err(SessionError::from);
        // Reading the turn back off the accumulator keeps the completed-versus-unanswered decision
        // in the one module that owns it. The single exception is the message the loop refuses
        // outright: a zero step budget builds no request, records nothing, and would otherwise make
        // the newest *seeded* turn look like this session's — which strict configuration already
        // rejects at startup, and which must not silently duplicate an exchange if it ever did not.
        let turn = match &outcome {
            Err(SessionError::Prompt(PromptError::ZeroSteps)) => None,
            _ => history.turns().last().cloned(),
        };
        (outcome, turn)
    })
    .await;

    let usage = usage.report();
    if usage.model_calls > 0
        && let Some(reports) = &runner.usage_reports
        && reports.try_send(usage).is_err()
    {
        // Informational accounting must never delay or fail a paid-for answer. A bounded full or
        // closed queue loses a live dashboard delta and leaves OTLP accounting unchanged.
        tracing::warn!(event = "gateway_usage_report_dropped");
    }

    let (outcome, turn) = match result {
        Ok(session) => session,
        Err(_) => {
            // The task itself died, so there is no history to trust and nothing to record.
            tracing::error!(event = "gateway_session_failed", category = "session-task");
            answer(replier, message, FAILURE_REPLY).await;
            return "failed";
        }
    };
    // The exchange when the session answered, and the bare question when it did not. The fixed
    // failure line is never stored: it is this daemon's sentence rather than the agent's, and
    // replaying it would teach the model to keep producing it.
    if let Some(window) = window
        && let Some(turn) = turn
    {
        runner
            .conversations
            .commit(&key, &granted, window, turn, &cache_key, Instant::now());
    }

    let answer_text = match outcome {
        Ok(outcome) => outcome.answer,
        Err(error) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category()
            );
            answer(replier, message, FAILURE_REPLY).await;
            return "failed";
        }
    };
    if answer(replier, message, &answer_text).await {
        "answered"
    } else {
        "reply-failed"
    }
}

/// Builds the credential-free meta view from gateway-owned catalog fields and the broker's fresh
/// subject-specific capability snapshot.
///
/// Deliberately takes no [`ModelConfig`], broker configuration, transport message, subject, or
/// principal. Those are exactly the places credentials, endpoints, paths, and identity live, and a
/// constructor that cannot receive them is stronger than one expected to remember to redact them.
fn agent_config_view(
    agent: &str,
    description: &str,
    model_class: Option<&str>,
    instructions: Option<&str>,
    limits: dekopon_agent::prompt::PromptLimits,
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
    BrokerLeg::connect_attested(
        client,
        TRACE_PREFIX,
        message.subject.clone(),
        route.agent.clone(),
    )
    .await
    .map_err(SessionError::from)
}

/// Sends one answer, reporting whether it arrived.
///
/// The outbound bound is applied here, once, rather than in each transport: a model writes this
/// text, every chat service rejects or mangles an oversized post, and one bound at the session
/// boundary is one place to read rather than three places to keep in agreement.
async fn answer(replier: &Arc<dyn ChatReplier>, message: &InboundMessage, text: &str) -> bool {
    match replier
        .reply(message.reply.clone(), bound_outbound(text))
        .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(event = "gateway_reply_failed", category = error.category());
            false
        }
    }
}

/// A session that could not run to completion.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("broker client could not be created")]
    BrokerClient(#[from] ClientError),
    #[error("broker leg could not be opened")]
    BrokerLeg(#[from] BrokerLegError),
    #[error(transparent)]
    Model(#[from] ModelError),
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
            Self::Model(_) => "model",
            Self::ChatGpt(_) => "chatgpt",
            Self::Prompt(error) => error.telemetry_kind(),
        }
    }
}
