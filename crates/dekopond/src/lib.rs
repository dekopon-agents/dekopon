//! The unprivileged Dekopon chat gateway and agent daemon.
//!
//! `dekopond` connects to chat services, waits efficiently for a wakeup, routes each authenticated
//! message to a named agent from the catalog, runs one bounded model session with the sandboxed
//! shell and safe on-demand meta tools, and replies with bounded text plus any images a provider
//! result attached unless an optional owned-thread continuation deliberately declines.
//!
//! # Authority
//!
//! It has none. It holds chat bot credentials and model credentials — the things it needs to hear a
//! question and to ask a model — and it never holds a provider credential, a policy, or an
//! authorization. Producing an image is a provider effect like any other: image bytes reach a reply
//! only through a route's `providerAttachments` opt-in, carried on a result the broker already
//! authorized and executed. Every provider effect a session drives is submitted to
//! `dekopon-brokerd` as an *attested* proposal naming the sender's canonical subject, and the
//! broker alone maps that subject to a principal, decides what it may do, and executes it. The
//! daemon's dependency set excludes every privileged broker crate: orchestration holds no effect
//! authority, and CI enforces it.
//!
//! Everything arriving from a chat service is untrusted, including the agent's own standing orders
//! from the catalog: neither can assert identity, name a principal, or widen a grant.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod asset;
mod cache_key;
mod config;
mod conversation;
mod progress;
mod routes;
mod session;
mod transport;

pub mod cli;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::Path,
    sync::Arc,
    time::Duration,
};

use dekopon_broker_protocol::{BrokerClient, ConversationKind};
use dekopon_config::LocalCatalog;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinSet, time::timeout};

pub use config::{
    CONFIG_API_VERSION, ConfigApiVersion, ConfigError, ConfigProblem, ConversationMatchConfig,
    DEFAULT_STOP_WORDS, DekopondConfig, HARD_MAX_CONFIG_BYTES, KeepAliveConfig, LivenessConfig,
    LivenessMode, LivenessOverride, LivenessSettings, MemoryConfig, MemoryPolicy, MemoryScope,
    MemoryWindow, ProgressSurface, ProviderAttachmentsConfig, ResolvedConfig, ResolvedRoute,
    ResolvedTelemetry, SlackExperience, SlackLivenessFallback, TelemetryConfig, TemplateOverrides,
    TransportConfig,
};
pub use routes::{RouteError, RouteProblem};
pub use session::SessionError;
pub use transport::TransportError;

use crate::{
    asset::AssetStore,
    config::render_problems,
    conversation::ConversationStore,
    routes::RoutingTable,
    session::{
        CancelOutcome, ConfiguredModels, ModelCache, ModelCredentialError, SessionGate,
        SessionRunner, model_bearer_token,
    },
    transport::{
        AssetFetcher, CancelRequest, ChatDriver, ChatTransport, InboundMessage, ThreadOwnership,
        TransportEvent, TransportIdentity, discord::DiscordTransport, local::LocalTransport,
        slack::SlackTransport, telegram::TelegramTransport, whatsapp::WhatsappTransport,
    },
};

/// Inbound messages buffered between the transport readers and the routing loop.
///
/// Bounded, so a chat service having a busy minute applies backpressure to the reader rather than
/// growing a queue the daemon can never work through. Admission control refuses the overflow with
/// a sentence, which is a better answer than an unbounded backlog.
const INBOUND_BUFFER: usize = 64;
/// How often a transport that ended for good is announced again while the daemon keeps serving.
///
/// A transport whose reader stops is gone until the process restarts, and the deployment has no
/// gateway probe: one error line at the moment it happened is a signal nobody is looking at an hour
/// later. Re-stating it on an interval is what lets an alert fire on the condition rather than on
/// catching the edge.
const TRANSPORT_HEALTH_INTERVAL: Duration = Duration::from_secs(60);
/// Independent fallback timeout for attachment inventories after their last message.
///
/// Persistent access dies earlier whenever its conversation generation does. This remains longer
/// than the default history timeout so a live reference normally stays resolvable; if this fallback
/// removes one first, its generation-owned number sequence still prevents a later file alias.
const ASSET_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The effective UID this daemon runs as, used for every ownership check.
#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Reads only the export settings, so the process can install its subscriber before serving.
///
/// [`run`] parses the same file and reports every configuration failure with full context, so a
/// caller that prefers that reporting can discard this error. This call decides one thing: whether
/// an OTLP layer is installed at all.
///
/// # Errors
///
/// Returns the same configuration errors [`run`] would.
pub async fn telemetry_settings(
    config_path: impl AsRef<Path>,
    uid: u32,
) -> Result<Option<ResolvedTelemetry>, DekopondError> {
    Ok(config::load(config_path, uid).await?.telemetry)
}

/// Loads configuration, connects every transport, and serves routed sessions until shutdown.
pub async fn run<F>(config_path: impl AsRef<Path>, shutdown: F) -> Result<(), DekopondError>
where
    F: Future<Output = ()> + Send,
{
    let uid = current_uid();
    let config = config::load(config_path, uid).await?;

    let catalog = LocalCatalog::load(&config.catalog_path).map_err(DekopondError::Catalog)?;
    let routes = Arc::new(RoutingTable::bind(&config, &catalog)?);
    let Prepared {
        transports: built_transports,
    } = prepare(&config, &routes)?;

    // One probe before anything connects, so "the broker is not running" is a startup failure with
    // a clear message rather than every session failing identically an hour later.
    let broker_client = BrokerClient::new(
        &config.broker.socket_path,
        config.broker.server_uid,
        config.broker.frame,
    )
    .map_err(DekopondError::BrokerProbe)?;
    let capabilities = broker_client
        .capabilities()
        .await
        .map_err(DekopondError::BrokerProbe)?;
    tracing::info!(
        event = "gateway_broker_ready",
        capability.count = capabilities.len()
    );
    let mut transports = Vec::with_capacity(config.transports.len());
    let mut identities = BTreeMap::new();
    let mut drivers: BTreeMap<String, Arc<dyn ChatDriver>> = BTreeMap::new();
    let mut asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>> = HashMap::new();
    let mut thread_ownership: HashMap<String, Arc<dyn ThreadOwnership>> = HashMap::new();
    let mut connect_problems = Vec::new();
    for (spec, mut transport) in config.transports.iter().zip(built_transports) {
        let identity = match transport.connect().await {
            Ok(identity) => identity,
            Err(source) => {
                connect_problems.push(TransportConnectProblem {
                    transport: spec.name().to_owned(),
                    source,
                });
                continue;
            }
        };
        tracing::info!(
            event = "gateway_transport_connected",
            transport = spec.name(),
            kind = spec.kind()
        );
        identities.insert(spec.name().to_owned(), identity);
        drivers.insert(spec.name().to_owned(), transport.driver());
        // Absent for a transport that carries no attachments, which is what makes the tool
        // unavailable on a route bound to one.
        if let Some(fetcher) = transport.asset_fetcher() {
            asset_fetchers.insert(spec.name().to_owned(), fetcher);
        }
        if let Some(ownership) = transport.thread_ownership() {
            thread_ownership.insert(spec.name().to_owned(), ownership);
        }
        transports.push(transport);
    }

    if !connect_problems.is_empty() {
        return Err(DekopondError::TransportConnect {
            problems: connect_problems,
        });
    }

    let runner = Arc::new(SessionRunner {
        broker: config.broker.clone(),
        models: Arc::new(ModelCache::new(Arc::new(ConfiguredModels))),
        gate: SessionGate::new(config.sessions.max_concurrent),
        reply_on_busy: config.sessions.reply_on_busy,
        conversations: ConversationStore::new(config.sessions.max_conversations),
        // Independently bounded for one-shot state; persistent access additionally carries the
        // conversation generation fence, so transcript invalidation retires its assets immediately.
        assets: Arc::new(AssetStore::new(
            config.sessions.max_conversations,
            ASSET_IDLE_TIMEOUT,
        )),
        asset_fetchers,
        liveness: config.liveness.clone(),
        thread_ownership,
        active_sessions: session::ActiveSessions::default(),
    });

    let (sender, receiver) = mpsc::channel::<TransportEvent>(INBOUND_BUFFER);
    let health = Arc::new(TransportHealth::new(transports.len()));
    let mut readers = JoinSet::new();
    for transport in transports {
        readers.spawn(read_transport(
            transport,
            sender.clone(),
            Arc::clone(&health),
        ));
    }
    drop(sender);
    let health_reporter = tokio::spawn(report_transport_health(Arc::clone(&health)));

    tracing::info!(
        event = "gateway_started",
        transport.count = config.transports.len(),
        route.count = routes.len()
    );

    let outcome = serve(
        runner,
        routes,
        Arc::new(identities),
        Arc::new(drivers),
        Arc::new(config.stop_words.clone()),
        receiver,
        shutdown,
        config.shutdown_grace,
    )
    .await;
    readers.abort_all();
    while readers.join_next().await.is_some() {}
    health_reporter.abort();
    // Awaiting an aborted handle is how the task is joined, not how it is checked: the only
    // answer it can give is the cancellation just asked for.
    #[allow(
        clippy::let_underscore_must_use,
        reason = "the JoinHandle was aborted on the line above, so its Result is the cancellation \
                  this shutdown requested rather than an outcome anything can act on"
    )]
    let _ = health_reporter.await;
    match outcome {
        ServeOutcome::Shutdown => {
            tracing::info!(event = "gateway_stopped", reason = "shutdown");
            Ok(())
        }
        // Nothing can wake the daemon again, and nobody asked it to stop. Exiting successfully
        // here is what let a gateway that lost every workspace to a revoked token look like a
        // clean run to whatever supervises it.
        ServeOutcome::TransportsLost => {
            tracing::error!(event = "gateway_stopped", reason = "transports-lost");
            Err(DekopondError::TransportsLost)
        }
    }
}

/// Why the routing loop stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServeOutcome {
    /// Somebody asked the daemon to stop.
    Shutdown,
    /// Every transport reader ended, so no message can reach the daemon again.
    TransportsLost,
}

/// The routing loop: one message in, at most one session task out.
#[allow(clippy::too_many_arguments)]
async fn serve<F>(
    runner: Arc<SessionRunner>,
    routes: Arc<RoutingTable>,
    identities: Arc<BTreeMap<String, TransportIdentity>>,
    drivers: Arc<BTreeMap<String, Arc<dyn ChatDriver>>>,
    stop_words: Arc<Vec<String>>,
    mut receiver: mpsc::Receiver<TransportEvent>,
    shutdown: F,
    grace: Duration,
) -> ServeOutcome
where
    F: Future<Output = ()> + Send,
{
    let mut sessions = JoinSet::new();
    tokio::pin!(shutdown);
    let mut outcome = ServeOutcome::Shutdown;

    loop {
        // Reaped opportunistically rather than awaited: a finished session's task must not hold a
        // slot in the set while the loop is blocked waiting for the next message.
        while let Some(result) = sessions.try_join_next() {
            observe_session(result);
        }
        tokio::select! {
            () = &mut shutdown => break,
            event = receiver.recv() => {
                let Some(event) = event else {
                    outcome = ServeOutcome::TransportsLost;
                    break;
                };
                match event {
                    TransportEvent::Message(message) => {
                        // Routing runs inside the transport's receive span, so a message dropped as
                        // unrouted or unaddressed says why inside its own trace instead of leaving
                        // an orphan debug record an operator cannot tie to anything.
                        let received = message.receive_span.clone();
                        received.in_scope(|| {
                            dispatch(
                                &runner,
                                &routes,
                                &identities,
                                &drivers,
                                &stop_words,
                                &mut sessions,
                                *message,
                            );
                        });
                    }
                    TransportEvent::CancelRequested(request) => cancel_session(&runner, &request),
                }
            }
        }
    }

    // In-flight sessions are given the configured grace to finish: a model call is already paid
    // for, and abandoning it means a person watching a chat window never hears back.
    if timeout(grace, async {
        while let Some(result) = sessions.join_next().await {
            observe_session(result);
        }
    })
    .await
    .is_err()
    {
        tracing::warn!(event = "gateway_sessions_abandoned");
        sessions.abort_all();
        while sessions.join_next().await.is_some() {}
    }
    outcome
}

/// Reports a session task that did not finish normally.
///
/// A session answers its own failures in chat and returns `()`, so reaching here means the task
/// itself panicked or was cancelled — a bug rather than a refusal, and the one session outcome
/// nobody in the conversation was told about.
fn observe_session(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::error!(event = "gateway_session_task_failed");
    }
}

fn dispatch(
    runner: &Arc<SessionRunner>,
    routes: &Arc<RoutingTable>,
    identities: &BTreeMap<String, TransportIdentity>,
    drivers: &BTreeMap<String, Arc<dyn ChatDriver>>,
    stop_words: &[String],
    sessions: &mut JoinSet<()>,
    message: InboundMessage,
) {
    let Some(route) = routes.route(&message) else {
        // Bots see ambient traffic. Silence is the correct answer, and debug level keeps a busy
        // channel from becoming the daemon's log volume. The conversation rides along because
        // "why did the bot not answer in here" is answered by which kind and which container the
        // route table did not claim — service identifiers, never the message.
        tracing::debug!(
            event = "gateway_message_ignored",
            transport = %message.transport,
            reason = "unrouted",
            conversation.kind = message.conversation.kind.as_str(),
            conversation.container = message.conversation.container.as_deref().unwrap_or_default()
        );
        return;
    };
    // Before the addressed check, because in a channel a stop word arrives as `<@U0123> stop` and
    // every unaddressed channel message is dropped below — which is where the matcher could never
    // see it. It fires only when this conversation has a session this sender started, so "stop"
    // said to an idle agent is still a question the agent answers.
    if transport::is_stop_word(
        identities.get(&message.transport),
        &message.text,
        stop_words,
    ) {
        let request = CancelRequest {
            transport: message.transport.clone(),
            conversation_id: message.conversation.key(),
            subject: message.subject.canonical(),
            via: dekopon_agent::CancelVia::StopReply,
        };
        match runner.active_sessions.cancel(&request) {
            CancelOutcome::Cancelled => {
                tracing::info!(
                    event = "gateway_session_stop_requested",
                    transport = %request.transport,
                    via = "stop-reply"
                );
                return;
            }
            // The session existed and this sender owned it; it simply finished first. Routing the
            // word as a question would answer a message that was never one.
            outcome @ CancelOutcome::AlreadyEnded => {
                tracing::debug!(
                    event = "gateway_session_stop_ignored",
                    transport = %request.transport,
                    reason = outcome.ignored_reason()
                );
                return;
            }
            CancelOutcome::NoSession | CancelOutcome::OtherSubject => {}
        }
    }
    // A channel route that fired on every message would be noise and cost. Shared conversations
    // therefore require an explicit address, except for one Slack Agent continuation that the
    // transport proved belongs to this authenticated sender in a freshly authorized owned thread.
    // A direct message is addressed by definition.
    let addressed = message.addressed.unwrap_or_else(|| {
        identities
            .get(&message.transport)
            .is_some_and(|identity| identity.is_addressed(&message.text))
    });
    let inherited_thread = message
        .thread_continuation
        .as_ref()
        .is_some_and(|continuation| continuation.inherited);
    // Every kind but a direct message is a shared conversation: a group DM, a channel, and a
    // thread under either all carry ambient traffic the bot must be summoned into.
    if message.conversation.kind != ConversationKind::DirectMessage
        && !addressed
        && !inherited_thread
    {
        tracing::debug!(
            event = "gateway_message_ignored",
            transport = %message.transport,
            reason = "not-addressed"
        );
        return;
    }
    let Some(driver) = drivers.get(&message.transport).cloned() else {
        tracing::error!(
            event = "gateway_message_ignored",
            transport = %message.transport,
            reason = "no-driver"
        );
        return;
    };
    sessions.spawn(session::run_session(
        Arc::clone(runner),
        route.clone(),
        message,
        driver,
    ));
}

/// Routes one authenticated cancel request to the session that owns the conversation.
///
/// Nothing is replied here. The session's own policy task owns the message on screen and writes
/// the ending, which is what keeps `Stopped.` from arriving ahead of the partial answer it follows.
fn cancel_session(runner: &Arc<SessionRunner>, request: &CancelRequest) {
    match runner.active_sessions.cancel(request).ignored_reason() {
        None => tracing::info!(
            event = "gateway_session_stop_requested",
            transport = %request.transport,
            via = via_label(request.via)
        ),
        // Acknowledged by the transport reader already; ignored here because only the subject that
        // started a session may stop it, and because a session that already ended has no work left.
        Some(reason) => tracing::debug!(
            event = "gateway_session_stop_ignored",
            transport = %request.transport,
            reason
        ),
    }
}

/// Stable low-cardinality label for how a cancel reached the daemon.
const fn via_label(via: dekopon_agent::CancelVia) -> &'static str {
    match via {
        dekopon_agent::CancelVia::NativeStop => "native-stop",
        dekopon_agent::CancelVia::Button => "button",
        dekopon_agent::CancelVia::StopReply => "stop-reply",
    }
}

/// One reader task per transport, feeding the routing loop.
async fn read_transport(
    mut transport: Box<dyn ChatTransport>,
    sender: mpsc::Sender<TransportEvent>,
    health: Arc<TransportHealth>,
) {
    loop {
        match transport.next().await {
            Ok(event) => {
                if sender.send(event).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                // A transport that cannot recover on its own ends its own reader. The alternative
                // is a hot loop against a service that is telling us to stop.
                tracing::error!(
                    event = "gateway_transport_stopped",
                    transport = transport.name(),
                    category = error.category()
                );
                // Recorded rather than only logged: this daemon keeps serving whatever is left,
                // so the condition outlives the line that reported it.
                health.mark_dead(transport.name());
                return;
            }
        }
    }
}

/// Which transports have ended for good, shared by the readers and the health reporter.
#[derive(Debug)]
struct TransportHealth {
    configured: usize,
    dead: std::sync::Mutex<BTreeSet<String>>,
}

impl TransportHealth {
    fn new(configured: usize) -> Self {
        Self {
            configured,
            dead: std::sync::Mutex::new(BTreeSet::new()),
        }
    }

    fn mark_dead(&self, transport: &str) {
        self.dead
            .lock()
            .expect("gateway transport health")
            .insert(transport.to_owned());
    }

    /// The dead transports by name, sorted so one line means the same thing every time.
    fn dead(&self) -> Vec<String> {
        self.dead
            .lock()
            .expect("gateway transport health")
            .iter()
            .cloned()
            .collect()
    }
}

/// Re-states a degraded transport set on an interval for as long as it stays degraded.
async fn report_transport_health(health: Arc<TransportHealth>) {
    let mut interval = tokio::time::interval(TRANSPORT_HEALTH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately once, and nothing can be dead before the readers start.
    interval.tick().await;
    loop {
        interval.tick().await;
        let dead = health.dead();
        if dead.is_empty() {
            continue;
        }
        // Configured transport names, which an operator wrote and telemetry already carries per
        // event. Nothing here comes from a chat service.
        tracing::warn!(
            event = "gateway_transports_degraded",
            transport.dead = dead.len(),
            transport.configured = health.configured,
            transports = %dead.join(",")
        );
    }
}

/// Resolves every credential this daemon holds and builds every transport, before any of them
/// authenticates.
///
/// Nothing here opens a socket or speaks to a chat service: it reads the owner-named environment
/// variables and constructs the fixed-endpoint clients. That split is the whole point. Reading a
/// token inside the connect loop meant a rollout missing two of them cost two crash loops, and the
/// first of those had already authenticated to the service whose token was present. This process
/// also cannot see a variable exported after it started, so an unset or blank one that used to
/// surface as a 401 on the first user's message is a startup refusal naming the variable instead.
///
/// Every problem is collected, so an operator who forgot two secrets in a deployment manifest is
/// told about both at once.
fn prepare(config: &ResolvedConfig, routes: &RoutingTable) -> Result<Prepared, DekopondError> {
    let mut problems = Vec::new();
    for model in routes.bound_models() {
        if let Err(source) = model_bearer_token(model) {
            problems.push(StartupProblem::ModelCredential(source));
        }
    }
    let mut transports = Vec::with_capacity(config.transports.len());
    for spec in &config.transports {
        match build_transport(spec) {
            Ok(transport) => transports.push(transport),
            Err(source) => problems.push(StartupProblem::Transport {
                transport: spec.name().to_owned(),
                source,
            }),
        }
    }
    if problems.is_empty() {
        Ok(Prepared { transports })
    } else {
        Err(DekopondError::Startup { problems })
    }
}

/// Everything `prepare` resolved, none of it having spoken to a chat service yet.
struct Prepared {
    /// One built transport per configured transport, in configuration order.
    transports: Vec<Box<dyn ChatTransport>>,
}

/// Reads one transport's owner-named credentials and builds its fixed-endpoint client.
///
/// Nothing here connects; `prepare` calls it for every transport before any of them does.
fn build_transport(spec: &TransportConfig) -> Result<Box<dyn ChatTransport>, TransportError> {
    Ok(match spec {
        TransportConfig::SlackSocketMode {
            name,
            app_token_env,
            bot_token_env,
            experience,
            liveness,
            endpoint,
            ..
        } => Box::new(SlackTransport::new(
            name.clone(),
            endpoint
                .clone()
                .unwrap_or_else(|| config::SLACK_ENDPOINT.to_owned()),
            transport::read_credential(app_token_env)?,
            transport::read_credential(bot_token_env)?,
            *experience,
            liveness.settings(),
        )?),
        TransportConfig::DiscordGateway {
            name,
            bot_token_env,
            liveness,
            endpoint,
            ..
        } => Box::new(DiscordTransport::new(
            name.clone(),
            endpoint
                .clone()
                .unwrap_or_else(|| config::DISCORD_ENDPOINT.to_owned()),
            transport::read_credential(bot_token_env)?,
            liveness.settings(),
        )?),
        TransportConfig::WhatsappCloudApi {
            name,
            app_secret_env,
            verify_token_env,
            access_token_env,
            bind,
            callback_path,
            waba_id,
            phone_number_id,
            graph_api_version,
            liveness,
            graph_endpoint,
        } => Box::new(WhatsappTransport::new(
            name.clone(),
            *bind,
            callback_path.clone(),
            waba_id.clone(),
            phone_number_id.clone(),
            graph_api_version.clone(),
            graph_endpoint
                .clone()
                .unwrap_or_else(|| config::WHATSAPP_GRAPH_ENDPOINT.to_owned()),
            transport::read_credential(app_secret_env)?,
            transport::read_credential(verify_token_env)?,
            transport::read_credential(access_token_env)?,
            liveness.settings(),
        )?),
        TransportConfig::TelegramLongPoll {
            name,
            bot_token_env,
            liveness,
            endpoint,
            ..
        } => Box::new(TelegramTransport::new(
            name.clone(),
            endpoint
                .clone()
                .unwrap_or_else(|| config::TELEGRAM_ENDPOINT.to_owned()),
            transport::read_credential(bot_token_env)?,
            liveness.settings(),
        )?),
        TransportConfig::Local {
            name,
            socket_path,
            liveness,
        } => Box::new(LocalTransport::new(
            name.clone(),
            socket_path.clone(),
            liveness.settings(),
        )),
    })
}

/// Startup or lifecycle failure.
#[derive(Debug, Error)]
pub enum DekopondError {
    /// Strict owner-controlled configuration failed.
    #[error("gateway configuration is invalid")]
    Config(#[from] ConfigError),
    /// The agent catalog could not be loaded or validated.
    #[error("gateway agent catalog is unavailable or invalid")]
    Catalog(#[source] dekopon_config::ConfigError),
    /// A route could not be bound to a catalog agent and a configured model.
    #[error("gateway route cannot be satisfied")]
    Route(#[from] RouteError),
    /// Something the daemon must hold before it serves is unusable; every one of them is named.
    #[error("{}", render_problems(.problems))]
    Startup {
        /// Every credential or client that could not be resolved, in the order they were tried.
        problems: Vec<StartupProblem>,
    },
    /// The configured broker did not answer a capability probe at startup.
    #[error("broker is not reachable; start dekopon-brokerd before the gateway")]
    BrokerProbe(#[source] dekopon_broker_protocol::ClientError),
    /// Every transport that could not authenticate or open its wakeup path.
    #[error("{}", render_problems(.problems))]
    TransportConnect {
        /// Transport names and connection failures, in configured order.
        problems: Vec<TransportConnectProblem>,
    },
    /// Every transport ended on its own, with no shutdown asked for.
    ///
    /// The daemon has no way left to hear a message, so it stops. Reporting it as a failure is the
    /// difference between a supervisor restarting the gateway and a pod that stays green.
    #[error("every chat transport ended; the gateway can no longer be reached")]
    TransportsLost,
}

/// A configured transport and its connection failure.
#[derive(Debug, Error)]
#[error("chat transport {transport} could not connect")]
pub struct TransportConnectProblem {
    /// Configured transport name included in the diagnostic.
    pub transport: String,
    /// The underlying transport failure.
    #[source]
    pub source: TransportError,
}

/// One thing the daemon must hold before any transport authenticates.
///
/// Resolved together by `prepare` and reported through [`DekopondError::Startup`], so a
/// deployment missing several secrets is one refusal naming all of them.
#[derive(Debug, Error)]
pub enum StartupProblem {
    /// A bound route's model names a credential variable nothing usable can be read from.
    #[error("configured model credential is unavailable")]
    ModelCredential(#[source] ModelCredentialError),
    /// A chat transport's credential is unusable, or its fixed-endpoint client could not be built.
    #[error("chat transport {transport} could not be prepared")]
    Transport {
        /// Configured transport name.
        transport: String,
        #[source]
        source: TransportError,
    },
}

#[cfg(test)]
mod tests;
