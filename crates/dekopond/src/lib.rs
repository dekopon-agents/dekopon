//! The daemon holds no provider authority; every effect is submitted to dekopon-brokerd as an
//! attested proposal, and nothing from chat, including the agent's own instructions, can assert
//! identity or widen a grant.

#![forbid(unsafe_code)]
#![cfg(unix)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
mod asset;
mod cache_key;
mod collection;
mod config;
mod conversation;
mod journal;
mod progress;
mod routes;
mod session;
mod transport;
mod wake;

pub mod cli;

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use dekopon_broker_protocol::{BrokerClient, ConversationKind};
use dekopon_config::LocalCatalog;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinSet, time::timeout};
use tracing::Instrument as _;

pub use config::{
    CONFIG_API_VERSION, ConfigApiVersion, ConfigError, ConfigProblem, ConversationMatchConfig,
    DEFAULT_FORGET_AFTER, DEFAULT_STOP_WORDS, DekopondConfig, HARD_MAX_CONFIG_BYTES, JournalConfig,
    KeepAliveConfig, LivenessConfig, LivenessMode, LivenessOverride, LivenessSettings,
    MemoryConfig, MemoryPolicy, MemoryScope, MemoryWindow, ProgressSurface, RecallSource,
    ResolvedConfig, ResolvedJournal, ResolvedRoute, ResolvedTelemetry, SlackExperience,
    SlackLivenessFallback, TelemetryConfig, TemplateOverrides, TransportConfig,
};
pub use routes::{RouteError, RouteProblem};
pub use session::SessionError;
pub use transport::TransportError;
pub use wake::WakeStoreError;

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
        AssetFetcher, CancelRequest, ChatDriver, ChatTransport, InboundMessage, MessageId,
        ThreadOwnership, TransportEvent, TransportIdentity, discord::DiscordTransport,
        local::LocalTransport, slack::SlackTransport, telegram::TelegramTransport,
        whatsapp::WhatsappTransport,
    },
};

const INBOUND_BUFFER: usize = 64;
const ASSET_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub async fn telemetry_settings(
    config_path: impl AsRef<Path>,
    uid: u32,
) -> Result<Option<ResolvedTelemetry>, DekopondError> {
    Ok(config::load(config_path, uid).await?.telemetry)
}

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
    let mut drivers: BTreeMap<String, Arc<dyn ChatDriver>> = BTreeMap::new();
    let mut asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>> = HashMap::new();
    let mut thread_ownership: HashMap<String, Arc<dyn ThreadOwnership>> = HashMap::new();
    let (sender, receiver) = mpsc::channel::<TransportEvent>(INBOUND_BUFFER);
    let mut readers = JoinSet::new();
    for transport in built_transports {
        let transport = transport::recovery::RecoveringTransport::new(transport);
        let name = transport.name().to_owned();
        drivers.insert(name.clone(), transport.driver());
        if let Some(fetcher) = transport.asset_fetcher() {
            asset_fetchers.insert(name.clone(), fetcher);
        }
        if let Some(ownership) = transport.thread_ownership() {
            thread_ownership.insert(name, ownership);
        }
        readers.spawn(read_transport(Box::new(transport), sender.clone()));
    }

    let journal = match config.journal.clone() {
        Some(journal) => Some(Arc::new(
            tokio::task::spawn_blocking(move || {
                journal::Journal::open(&journal.dir, journal.max_bytes)
            })
            .await
            .map_err(DekopondError::TransportTask)?
            .map_err(|error| DekopondError::Journal {
                kind: match error {
                    journal::JournalError::Io { kind } => kind,
                    journal::JournalError::Corrupt => std::io::ErrorKind::InvalidData,
                },
            })?,
        )),
        None => None,
    };
    let wakes = match config.wakes.clone() {
        Some(wakes) => Some(Arc::new(
            tokio::task::spawn_blocking(move || wake::WakeStore::open(&wakes))
                .await
                .map_err(DekopondError::TransportTask)??,
        )),
        None => None,
    };
    let runner = Arc::new(SessionRunner {
        broker: config.broker.clone(),
        models: Arc::new(ModelCache::new(Arc::new(ConfiguredModels::default()))),
        gate: SessionGate::new(config.sessions.max_concurrent),
        reply_on_busy: config.sessions.reply_on_busy,
        conversations: ConversationStore::new(config.sessions.max_conversations),
        journal,
        assets: Arc::new(AssetStore::with_retention(
            config.sessions.max_conversations,
            ASSET_IDLE_TIMEOUT,
            config.sessions.asset_retention_bytes,
        )),
        asset_fetchers,
        liveness: config.liveness.clone(),
        thread_ownership,
        active_sessions: session::ActiveSessions::new(config.sessions.max_concurrent),
        wakes,
    });

    tracing::info!(
        event = "gateway_started",
        transport.count = config.transports.len(),
        route.count = routes.len()
    );

    let mut terminal = Ok(());
    let stopped = async {
        terminal = supervise_transports(&mut readers, shutdown).await;
        readers.abort_all();
    };
    let outcome = serve(
        runner,
        routes,
        Arc::new(BTreeMap::new()),
        Arc::new(drivers),
        Arc::new(config.stop_words.clone()),
        receiver,
        stopped,
        config.shutdown_grace,
        collection::Collector::new(&config.transports, config.sessions.max_concurrent),
    )
    .await;
    readers.abort_all();
    while let Some(result) = readers.join_next().await {
        match result {
            Ok(Err(problem)) => tracing::warn!(
                event = "gateway_transport_stopped",
                transport = %problem.transport,
                category = problem.source.category(),
            ),
            Err(error) if !error.is_cancelled() => tracing::error!(
                event = "gateway_transport_task_failed", error = %error,
            ),
            Ok(Ok(())) | Err(_) => {}
        }
    }
    drop(sender);
    if let Err(error) = terminal {
        tracing::error!(event = "gateway_stopped", reason = "transport-failed");
        return Err(error);
    }
    match outcome {
        ServeOutcome::Shutdown => {
            tracing::info!(event = "gateway_stopped", reason = "shutdown");
            Ok(())
        }
        // Exiting successfully here would let a gateway that lost every workspace to a revoked
        // token look like a clean run to whatever supervises it, so this is reported as a failure
        // instead.
        ServeOutcome::TransportsLost => {
            tracing::error!(event = "gateway_stopped", reason = "transports-lost");
            Err(DekopondError::TransportsLost)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServeOutcome {
    Shutdown,
    TransportsLost,
}

#[allow(clippy::too_many_arguments)]
async fn serve<F>(
    runner: Arc<SessionRunner>,
    routes: Arc<RoutingTable>,
    mut identities: Arc<BTreeMap<String, TransportIdentity>>,
    drivers: Arc<BTreeMap<String, Arc<dyn ChatDriver>>>,
    stop_words: Arc<Vec<String>>,
    mut receiver: mpsc::Receiver<TransportEvent>,
    shutdown: F,
    grace: Duration,
    mut collector: collection::Collector,
) -> ServeOutcome
where
    F: Future<Output = ()> + Send,
{
    let mut sessions = JoinSet::new();
    let mut ticks = JoinSet::new();
    let mut wakes = runner.wakes.clone();
    tokio::pin!(shutdown);
    let mut outcome = ServeOutcome::Shutdown;

    loop {
        while let Some(result) = sessions.try_join_next() {
            observe_session(result);
        }
        let deadline = collector.deadline();
        let wake_deadline = wakes.as_ref().and_then(|store| store.next_at()).map(|at| {
            tokio::time::Instant::now() + at.duration_since(SystemTime::now()).unwrap_or_default()
        });
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = async {
                match wake_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(store) = wakes.clone() else { continue };
                let reading = Arc::clone(&store);
                let due = tokio::task::spawn_blocking(move || reading.take_due(SystemTime::now())).await;
                match due {
                    Ok(Ok(due)) => {
                        for fired in due.fired {
                            start_wake(&runner, &routes, &drivers, &mut sessions, fired);
                        }
                        for tick in due.ticks {
                            spawn_tick(&runner, &routes, &store, &mut ticks, tick);
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::error!(event = "gateway_wake_store_failed", error = %error, wakes = "stopped");
                        wakes = None;
                    }
                    Err(_) => {
                        tracing::error!(event = "gateway_wake_store_failed", wakes = "stopped");
                        wakes = None;
                    }
                }
            },
            Some(result) = ticks.join_next() => {
                match result {
                    Ok(Some(fired)) => start_wake(&runner, &routes, &drivers, &mut sessions, fired),
                    Ok(None) => {}
                    Err(_) => tracing::error!(event = "gateway_wake_task_failed"),
                }
            },
            () = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                for message in collector.take_due(tokio::time::Instant::now()) {
                    start_session(&runner, &routes, &drivers, &mut sessions, message);
                }
            },
            event = receiver.recv() => {
                let Some(event) = event else {
                    outcome = ServeOutcome::TransportsLost;
                    break;
                };
                match event {
                    TransportEvent::Connected { name, identity } => {
                        Arc::make_mut(&mut identities).insert(name, identity);
                    }
                    TransportEvent::Message(message) => {
                        let received = message.receive_span.clone();
                        received.in_scope(|| {
                            dispatch(
                                &runner,
                                &routes,
                                &identities,
                                &drivers,
                                &stop_words,
                                &mut sessions,
                                &mut collector,
                                *message,
                            );
                        });
                    }
                    TransportEvent::CancelRequested(request) => {
                        if collector.cancel(&request) {
                            tracing::info!(event = "gateway_input_stop_requested", transport = %request.transport, via = via_label(request.via));
                        }
                        cancel_session(&runner, &request);
                    },
                }
            }
        }
    }

    collector.shutdown();

    // A tick that fires here has already retired its row, so its wake must still start.
    if timeout(grace, async {
        while let Some(result) = ticks.join_next().await {
            if let Ok(Some(fired)) = result {
                start_wake(&runner, &routes, &drivers, &mut sessions, fired);
            }
        }
        while let Some(result) = sessions.join_next().await {
            observe_session(result);
        }
    })
    .await
    .is_err()
    {
        tracing::warn!(event = "gateway_sessions_abandoned");
        // A blocking probe cannot be aborted; the runtime's shutdown timeout bounds it.
        ticks.detach_all();
        sessions.abort_all();
        while sessions.join_next().await.is_some() {}
    }
    outcome
}

fn observe_session(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::error!(event = "gateway_session_task_failed");
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    runner: &Arc<SessionRunner>,
    routes: &Arc<RoutingTable>,
    identities: &BTreeMap<String, TransportIdentity>,
    drivers: &BTreeMap<String, Arc<dyn ChatDriver>>,
    stop_words: &[String],
    sessions: &mut JoinSet<()>,
    collector: &mut collection::Collector,
    mut message: InboundMessage,
) {
    let Some((route_id, route)) = routes.route_index(&message) else {
        tracing::debug!(
            event = "gateway_message_ignored",
            transport = %message.transport,
            reason = "unrouted",
            conversation.kind = message.conversation.kind.as_str(),
            conversation.container = message.conversation.container.as_deref().unwrap_or_default()
        );
        return;
    };
    // Checked before the addressed filter, since a channel stop word like the bot mention plus stop
    // would otherwise be dropped as unaddressed before the matcher ever saw it; it only fires for a
    // session this sender started.
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
        let pending_cancelled = collector.cancel(&request);
        let active_outcome = runner.active_sessions.cancel(&request);
        if pending_cancelled {
            if matches!(
                active_outcome,
                CancelOutcome::NoSession | CancelOutcome::OtherSubject | CancelOutcome::Completing
            ) && let Some(driver) = drivers.get(&message.transport).cloned()
                && let Some(reply) = runner.gate.refusal()
            {
                let receipt = message.receive_span.clone();
                sessions.spawn(
                    async move {
                        let _reply = reply;
                        session::answer(&driver, &message, session::STOPPED_REPLY).await;
                    }
                    .instrument(receipt),
                );
            }
            return;
        }
        match active_outcome {
            CancelOutcome::Cancelled => {
                tracing::info!(
                    event = "gateway_session_stop_requested",
                    transport = %request.transport,
                    via = "stop-reply"
                );
                return;
            }
            outcome @ (CancelOutcome::AlreadyCancelled | CancelOutcome::Completing) => {
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
    let addressed = message.addressed.unwrap_or_else(|| {
        identities
            .get(&message.transport)
            .is_some_and(|identity| identity.is_addressed(&message.text))
    });
    let inherited_thread = message
        .thread_continuation
        .as_ref()
        .is_some_and(|continuation| continuation.inherited);
    if message.conversation.kind != ConversationKind::DirectMessage
        && !addressed
        && !inherited_thread
        && !collector.is_native_continuation(route_id, &message)
    {
        tracing::debug!(
            event = "gateway_message_ignored",
            transport = %message.transport,
            reason = "not-addressed"
        );
        return;
    }
    message.late_photos = runner.active_sessions.late_photos(route, &message);
    match collector.offer(route_id, message) {
        collection::Offered::Pending => {}
        collection::Offered::Immediate(message) => {
            start_session(runner, routes, drivers, sessions, message)
        }
        collection::Offered::Refused(message, reason) => {
            collection::disposition(&message.receive_span, reason);
            if let Some(driver) = drivers.get(&message.transport).cloned()
                && let Some(permit) = runner.gate.refusal()
            {
                let receipt = message.receive_span.clone();
                sessions.spawn(async move {
                    let _permit = permit;
                    let reply = match reason {
                        "late-instructions" => "Your instruction was not processed. Please send it after the current request completes; the earlier photos are still being collected.",
                        "different-run" => "This input was not processed because another request's photos are still being collected. Please send it separately after that request completes.",
                        "collection-full" => "Busy collecting other requests. Please try again shortly.",
                        "incompatible-group" => "Another media group is still being collected. Please retry this group separately.",
                        "deadline-overflow" => "Input refused: the configured media collection deadline cannot be represented.",
                        _ => "Input refused: too many attachments or messages, or too much text. Please send a smaller request.",
                    };
                    session::answer(&driver, &message, reply).await;
                }.instrument(receipt));
            }
        }
    }
}

fn start_session(
    runner: &Arc<SessionRunner>,
    routes: &RoutingTable,
    drivers: &BTreeMap<String, Arc<dyn ChatDriver>>,
    sessions: &mut JoinSet<()>,
    message: InboundMessage,
) {
    let Some(route) = routes.route(&message) else {
        collection::disposition(&message.receive_span, "route-lost");
        return;
    };
    let Some(driver) = drivers.get(&message.transport).cloned() else {
        message
            .receive_span
            .in_scope(|| tracing::error!(event = "gateway_message_ignored", reason = "no-driver"));
        collection::disposition(&message.receive_span, "no-driver");
        return;
    };
    sessions.spawn(session::run_session(
        Arc::clone(runner),
        route.clone(),
        message,
        driver,
    ));
}

/// A wake whose route no longer answers as its agent is delivered as its note, so the person still
/// hears what they asked to be reminded of.
fn start_wake(
    runner: &Arc<SessionRunner>,
    routes: &RoutingTable,
    drivers: &BTreeMap<String, Arc<dyn ChatDriver>>,
    sessions: &mut JoinSet<()>,
    fired: wake::Fired,
) {
    let agent = fired.agent().clone();
    let id = fired.id();
    let message = fired.into_inbound();
    let answering = routes
        .route(&message)
        .is_some_and(|route| route.wakes && route.agent == agent);
    let receipt = message.receive_span.clone();
    receipt.in_scope(|| tracing::info!(event = "gateway_wake_fired", wake.id = %id));
    if answering {
        start_session(runner, routes, drivers, sessions, message);
        return;
    }
    receipt.in_scope(|| tracing::warn!(event = "gateway_wake_orphaned", wake.id = %id));
    if let Some(driver) = drivers.get(&message.transport).cloned()
        && let MessageId::Wake { notice, .. } = &message.message_id
    {
        let notice = notice.clone();
        sessions.spawn(
            async move {
                session::answer(&driver, &message, &notice).await;
            }
            .instrument(receipt),
        );
    }
}

fn spawn_tick(
    runner: &Arc<SessionRunner>,
    routes: &RoutingTable,
    store: &Arc<wake::WakeStore>,
    ticks: &mut JoinSet<Option<wake::Fired>>,
    tick: wake::Tick,
) {
    let store = Arc::clone(store);
    // No transport is named "wake", so a tick's admission key never collides with a session's.
    let Some(admission) = runner
        .gate
        .admit(("wake".to_owned(), tick.id().to_string()))
    else {
        tracing::info!(event = "gateway_wake_tick_skipped", wake.id = %tick.id(), reason = "busy");
        return;
    };
    let limits = routes
        .route_for_anchor(tick.anchor())
        .map(|route| dekopon_shell::Limits {
            max_capability_calls: route.limits.max_capability_calls,
            timeout: route.script_timeout,
            ..dekopon_shell::Limits::default()
        });
    let broker = runner.broker.clone();
    let runtime = tokio::runtime::Handle::current();
    ticks.spawn_blocking(move || {
        let _admission = admission;
        wake::run_tick(tick, &store, &broker, &runtime, limits)
    });
}

fn cancel_session(runner: &Arc<SessionRunner>, request: &CancelRequest) {
    match runner.active_sessions.cancel(request).ignored_reason() {
        None => tracing::info!(
            event = "gateway_session_stop_requested",
            transport = %request.transport,
            via = via_label(request.via)
        ),
        // Ignored here since only the subject that started a session may stop it, and a session
        // that already ended has nothing left to stop.
        Some(reason) => tracing::debug!(
            event = "gateway_session_stop_ignored",
            transport = %request.transport,
            reason
        ),
    }
}

const fn via_label(via: dekopon_agent::CancelVia) -> &'static str {
    match via {
        dekopon_agent::CancelVia::NativeStop => "native-stop",
        dekopon_agent::CancelVia::Button => "button",
        dekopon_agent::CancelVia::StopReply => "stop-reply",
    }
}

async fn read_transport(
    mut transport: Box<dyn ChatTransport>,
    sender: mpsc::Sender<TransportEvent>,
) -> Result<(), TransportConnectProblem> {
    let result = async {
        let identity = transport.connect().await?;
        if sender
            .send(TransportEvent::Connected {
                name: transport.name().to_owned(),
                identity,
            })
            .await
            .is_err()
        {
            return Ok(());
        }
        loop {
            let event = transport.next().await?;
            if sender.send(event).await.is_err() {
                return Ok(());
            }
        }
    }
    .await;
    result.map_err(|source| TransportConnectProblem {
        transport: transport.name().to_owned(),
        source,
    })
}

async fn supervise_transports<F>(
    readers: &mut JoinSet<Result<(), TransportConnectProblem>>,
    shutdown: F,
) -> Result<(), DekopondError>
where
    F: Future<Output = ()> + Send,
{
    tokio::select! {
        biased;
        () = shutdown => Ok(()),
        result = readers.join_next() => match result {
            Some(Ok(Err(problem))) => Err(DekopondError::TransportConnect { problems: vec![problem] }),
            Some(Err(source)) => Err(DekopondError::TransportTask(source)),
            Some(Ok(Ok(()))) | None => Err(DekopondError::TransportsLost),
        }
    }
}

fn model_credential_problems<'a>(
    models: impl IntoIterator<Item = &'a config::ModelConfig>,
    mut resolve: impl FnMut(
        &config::ModelConfig,
    ) -> Result<Option<String>, session::ModelCredentialError>,
) -> Vec<StartupProblem> {
    models
        .into_iter()
        .filter_map(|model| resolve(model).err().map(StartupProblem::ModelCredential))
        .collect()
}

#[cfg(test)]
mod openrouter_startup_tests {
    use super::*;

    #[test]
    fn missing_and_blank_openrouter_credentials_are_startup_problems_not_config_problems() {
        let model = config::ModelConfig::Openrouter {
            name: "router".into(),
            model: "vendor/model".into(),
            api_key_env: "OPENROUTER_API_KEY".into(),
            timeout_ms: 1000,
            classes: Vec::new(),
            modalities: Vec::new(),
            generation: None,
            reasoning: None,
            routing: None,
            cache: None,
        };
        for value in [None, Some(std::ffi::OsString::from("  "))] {
            let problems = model_credential_problems([&model], |model| {
                session::model_bearer_token_with(model, |variable| {
                    assert_eq!(variable, "OPENROUTER_API_KEY");
                    value.clone()
                })
            });
            assert!(
                matches!(problems.as_slice(), [StartupProblem::ModelCredential(error)] if error.variable == "OPENROUTER_API_KEY" && error.model == "router")
            );
        }
        let problems = model_credential_problems([&model], |model| {
            session::model_bearer_token_with(model, |_| Some("synthetic-key".into()))
        });
        assert!(problems.is_empty());
    }
}

fn prepare(config: &ResolvedConfig, routes: &RoutingTable) -> Result<Prepared, DekopondError> {
    let mut problems = model_credential_problems(routes.bound_models(), model_bearer_token);
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

struct Prepared {
    transports: Vec<Box<dyn ChatTransport>>,
}

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
            ..
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

#[derive(Debug, Error)]
pub enum DekopondError {
    #[error("gateway configuration is invalid")]
    Config(#[from] ConfigError),
    #[error("gateway agent catalog is unavailable or invalid")]
    Catalog(#[source] dekopon_config::ConfigError),
    #[error("gateway route cannot be satisfied")]
    Route(#[from] RouteError),
    #[error("{}", render_problems(.problems))]
    Startup { problems: Vec<StartupProblem> },
    #[error("broker is not reachable; start dekopon-brokerd before the gateway")]
    BrokerProbe(#[source] dekopon_broker_protocol::ClientError),
    #[error("{}", render_problems(.problems))]
    TransportConnect {
        problems: Vec<TransportConnectProblem>,
    },
    #[error("chat transport task failed")]
    TransportTask(#[source] tokio::task::JoinError),
    #[error("every chat transport ended; the gateway can no longer be reached")]
    TransportsLost,
    #[error("conversation journal directory is unusable: {kind}")]
    Journal { kind: std::io::ErrorKind },
    #[error(transparent)]
    WakeStore(#[from] WakeStoreError),
}

#[derive(Debug, Error)]
#[error("chat transport {transport} failed")]
pub struct TransportConnectProblem {
    pub transport: String,
    #[source]
    pub source: TransportError,
}

#[derive(Debug, Error)]
pub enum StartupProblem {
    #[error("configured model credential is unavailable")]
    ModelCredential(#[source] ModelCredentialError),
    #[error("chat transport {transport} could not be prepared")]
    Transport {
        transport: String,
        #[source]
        source: TransportError,
    },
}

#[cfg(test)]
mod tests;
