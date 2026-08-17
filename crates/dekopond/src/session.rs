//! One routed message, from admission through the answer that goes back to chat.
//!
//! A session holds no authority. It opens an *attested* broker leg naming the sender, and whatever
//! that leg reports as granted is what the broker decided the sender may reach through this agent.
//! An empty grant ends the session before a single model token is spent, which is deliberate: the
//! cheapest possible refusal, and one that cannot be talked out of by the message text.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use dekopon_agent::{
    BrokerLeg, BrokerLegError, ShellRuntime,
    prompt::{PromptError, run_prompt},
};
use dekopon_broker_protocol::{BrokerClient, ClientError, ERROR_UNAUTHENTICATED};
use dekopon_model::{
    chatgpt::ChatGptCodexModel,
    model::{ChatModel, ModelError, OpenAiChatModel},
};
use dekopon_shell::{CapabilityInvoker as _, Limits as ShellLimits};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::Instrument as _;

use crate::{
    config::{ModelConfig, ResolvedBroker},
    routes::BoundRoute,
    transport::{ChatReplier, InboundMessage, bound_outbound},
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

/// One conversation, for in-flight serialization.
type ConversationKey = (String, String, Option<String>);

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
    in_flight: Arc<Mutex<BTreeSet<ConversationKey>>>,
}

impl SessionGate {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent)),
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Admits one session, or reports that this message must be refused.
    pub fn admit(&self, key: ConversationKey) -> Option<SessionAdmission> {
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
    key: ConversationKey,
    in_flight: Arc<Mutex<BTreeSet<ConversationKey>>>,
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

    let outcome = session(&runner, &route, &message, &replier)
        .instrument(tracing::info_span!("gateway.session", agent = %route.agent))
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
    // The authorization gate, and it costs nothing: the broker already answered
    // `capabilitiesFor` with what this subject may reach through this agent. An empty answer is a
    // complete answer, so there is no model call to make.
    if leg.granted().is_empty() {
        tracing::info!(event = "gateway_session_rejected", reason = "unauthorized");
        answer(replier, message, UNAUTHORIZED_REPLY).await;
        return "unauthorized";
    }

    let model_config = Arc::clone(&route.model);
    let models = Arc::clone(&runner.models);
    let limits = route.limits;
    let instructions = route.instructions.clone();
    let text = message.text.clone();
    let shell = ShellLimits {
        max_capability_calls: limits.max_capability_calls,
        ..ShellLimits::default()
    };
    // The prompt loop and the interpreter are both synchronous and both can block for a long time
    // — a model round trip, a script that sleeps, a broker call per command. Running that on a
    // runtime worker would stall every other session in the process.
    let span = tracing::Span::current();
    let result = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        let model = models.build(&model_config)?;
        let runtime = ShellRuntime {
            invoker: leg,
            limits: shell,
            curl_capability: None,
        };
        run_prompt(
            model.as_ref(),
            &runtime,
            &text,
            instructions.as_deref(),
            limits,
        )
        .map_err(SessionError::from)
    })
    .await;

    let answer_text = match result {
        Ok(Ok(outcome)) => outcome.answer,
        Ok(Err(error)) => {
            tracing::error!(
                event = "gateway_session_failed",
                category = error.category()
            );
            answer(replier, message, FAILURE_REPLY).await;
            return "failed";
        }
        Err(_) => {
            tracing::error!(event = "gateway_session_failed", category = "session-task");
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
