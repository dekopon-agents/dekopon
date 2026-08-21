//! The unprivileged Dekopon chat gateway and agent daemon.
//!
//! `dekopond` connects to chat services, waits efficiently for a wakeup, routes each authenticated
//! message to a named agent from the catalog, runs one bounded model session with the sandboxed
//! shell and safe on-demand meta tools, and replies with bounded text plus an optional generated
//! image unless an optional owned-thread continuation deliberately declines.
//!
//! # Authority
//!
//! It has none. It holds chat bot credentials and model credentials — the things it needs to hear a
//! question and to ask a model — and it never holds a provider credential, a policy, or an
//! authorization. Explicit route-scoped image generation is model inference inside this
//! unprivileged boundary, with no provider or broker credential. Every provider effect a session
//! drives is submitted to `dekopon-brokerd` as an *attested*
//! proposal naming the sender's canonical subject, and the broker alone maps that subject to a
//! principal, decides what it may do, and executes it. The daemon's dependency set excludes every
//! privileged broker crate for the same reason `dekopon-run`'s does, and CI enforces it.
//!
//! Everything arriving from a chat service is untrusted, including the agent's own standing orders
//! from the catalog: neither can assert identity, name a principal, or widen a grant.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod activity;
mod asset;
mod cache_key;
mod config;
mod conversation;
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

use dekopon_broker_protocol::{
    AgentInventory, BrokerClient, MAX_REPORTED_AGENT_CAPABILITIES, MAX_REPORTED_AGENT_PROVIDERS,
    MAX_REPORTED_AGENTS, MAX_REPORTED_PERMISSIONS, MAX_REPORTED_TEXT_BYTES, ModelUsageReport,
    ReportedAgent, ReportedAgentCapability,
};
use dekopon_config::LocalCatalog;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinSet, time::timeout};

pub use config::{
    ActivityMode, CONFIG_API_VERSION, ConfigApiVersion, ConfigError, ConversationConfig,
    ConversationPolicy, ConversationWindow, DekopondConfig, HARD_MAX_CONFIG_BYTES,
    ImageGeneratorConfig, NativeActivityConfig, ResolvedConfig, ResolvedRoute, ResolvedTelemetry,
    SlackActivityConfig, SlackActivityFallback, SlackExperience, SocketDiscovery, TelemetryConfig,
    TransportConfig,
};
pub use routes::RouteError;
pub use session::SessionError;
pub use transport::TransportError;

use crate::{
    asset::AssetStore,
    conversation::ConversationStore,
    routes::RoutingTable,
    session::{
        ConfiguredModels, ImageGeneratorStartupError, STOPPED_REPLY, SessionGate, SessionRunner,
        configured_image_generators,
    },
    transport::{
        AssetFetcher, ChatActivity, ChatReplier, ChatTransport, ConversationKind, InboundMessage,
        OutboundReply, SessionStop, ThreadOwnership, TransportEvent, TransportIdentity,
        discord::DiscordTransport, local::LocalTransport, slack::SlackTransport,
        telegram::TelegramTransport, whatsapp::WhatsappTransport,
    },
};

/// Inbound messages buffered between the transport readers and the routing loop.
///
/// Bounded, so a chat service having a busy minute applies backpressure to the reader rather than
/// growing a queue the daemon can never work through. Admission control refuses the overflow with
/// a sentence, which is a better answer than an unbounded backlog.
const INBOUND_BUFFER: usize = 64;
/// Informational model-usage deltas waiting to be coalesced for the broker-hosted web UI.
const USAGE_REPORT_BUFFER: usize = 64;
/// Informational reporting must not delay gateway startup, an answer, or shutdown.
const STATUS_REPORT_TIMEOUT: Duration = Duration::from_secs(2);
/// Re-publishes static inventory so a restarted broker recovers its in-memory view.
const STATUS_INVENTORY_INTERVAL: Duration = Duration::from_secs(60);

/// How long a conversation's attachments stay addressable after its last message.
///
/// Longer than a route's default conversation idle timeout, because the reference lines live in
/// replayed history and a number that resolved a minute ago should not stop resolving while the
/// text naming it is still in the prompt.
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
    // Span verbosity is process state rather than a parameter because it describes the deployment.
    // Set before any message is routed so none is recorded under the wrong mode.
    dekopon_core::set_telemetry_payloads(
        config
            .telemetry
            .as_ref()
            .is_some_and(|telemetry| telemetry.telemetry_payloads),
    );

    let catalog = LocalCatalog::load(&config.catalog_path).map_err(DekopondError::Catalog)?;
    let routes = Arc::new(RoutingTable::bind(&config, &catalog)?);
    // Resolve model credentials and construct the fixed-endpoint clients before a chat transport
    // accepts work. A route naming image generation must not start as a tool that can only fail.
    let referenced_image_generators = config
        .routes
        .iter()
        .filter_map(|route| route.image_generator.clone())
        .collect::<BTreeSet<_>>();
    let image_generators =
        configured_image_generators(&config.image_generators, &referenced_image_generators)
            .map_err(DekopondError::ImageGenerator)?;
    let inventory = agent_inventory(&catalog);
    let heartbeat_inventory = inventory.clone();

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
    match timeout(
        STATUS_REPORT_TIMEOUT,
        broker_client.publish_agent_inventory(inventory),
    )
    .await
    {
        Ok(Ok(())) => tracing::info!(event = "gateway_agent_inventory_reported"),
        Ok(Err(_)) | Err(_) => tracing::warn!(event = "gateway_agent_inventory_report_failed"),
    }

    let mut transports = Vec::with_capacity(config.transports.len());
    let mut identities = BTreeMap::new();
    let mut repliers: BTreeMap<String, Arc<dyn ChatReplier>> = BTreeMap::new();
    let mut asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>> = HashMap::new();
    let mut activities: HashMap<String, Arc<dyn ChatActivity>> = HashMap::new();
    let mut thread_ownership: HashMap<String, Arc<dyn ThreadOwnership>> = HashMap::new();
    for spec in &config.transports {
        let mut transport = build_transport(spec)?;
        let identity =
            transport
                .connect()
                .await
                .map_err(|source| DekopondError::TransportConnect {
                    transport: spec.name().to_owned(),
                    source,
                })?;
        tracing::info!(
            event = "gateway_transport_connected",
            transport = spec.name(),
            kind = spec.kind()
        );
        identities.insert(spec.name().to_owned(), identity);
        repliers.insert(spec.name().to_owned(), transport.replier());
        // Absent for a transport that carries no attachments, which is what makes the tool
        // unavailable on a route bound to one.
        if let Some(fetcher) = transport.asset_fetcher() {
            asset_fetchers.insert(spec.name().to_owned(), fetcher);
        }
        if let Some(activity) = transport.activity() {
            activities.insert(spec.name().to_owned(), activity);
        }
        if let Some(ownership) = transport.thread_ownership() {
            thread_ownership.insert(spec.name().to_owned(), ownership);
        }
        transports.push(transport);
    }

    let (usage_sender, usage_receiver) = mpsc::channel(USAGE_REPORT_BUFFER);
    let mut usage_reporter = tokio::spawn(report_status(
        config.broker.clone(),
        heartbeat_inventory,
        usage_receiver,
    ));
    let runner = Arc::new(SessionRunner {
        broker: config.broker.clone(),
        models: Arc::new(ConfiguredModels),
        gate: SessionGate::new(config.sessions.max_concurrent),
        reply_on_busy: config.sessions.reply_on_busy,
        conversations: ConversationStore::new(config.sessions.max_conversations),
        // Sized and expired like the conversation store, because an attachment reference outliving
        // the conversation that introduced it is a number no prompt can still name.
        assets: Arc::new(AssetStore::new(
            config.sessions.max_conversations,
            ASSET_IDLE_TIMEOUT,
        )),
        asset_fetchers,
        image_generators,
        activities,
        thread_ownership,
        active_sessions: session::ActiveSessions::default(),
        usage_reports: Some(usage_sender),
    });

    let (sender, receiver) = mpsc::channel::<TransportEvent>(INBOUND_BUFFER);
    let mut readers = JoinSet::new();
    for transport in transports {
        readers.spawn(read_transport(transport, sender.clone()));
    }
    drop(sender);

    tracing::info!(
        event = "gateway_started",
        transport.count = config.transports.len(),
        route.count = routes.len()
    );

    serve(
        runner,
        routes,
        Arc::new(identities),
        Arc::new(repliers),
        receiver,
        shutdown,
        config.shutdown_grace,
    )
    .await;
    readers.abort_all();
    while readers.join_next().await.is_some() {}
    if timeout(STATUS_REPORT_TIMEOUT, &mut usage_reporter)
        .await
        .is_err()
    {
        usage_reporter.abort();
        #[allow(
            clippy::let_underscore_must_use,
            reason = "reaping a handle this line just aborted, which yields JoinError::Cancelled; \
                      a reporter that had failed on its own would have completed the timeout above"
        )]
        let _ = usage_reporter.await;
        tracing::warn!(event = "gateway_usage_reporter_abandoned");
    }

    tracing::info!(event = "gateway_stopped");
    Ok(())
}

fn agent_inventory(catalog: &LocalCatalog) -> AgentInventory {
    let mut truncated = catalog.agents().len() > MAX_REPORTED_AGENTS;
    let agents = catalog
        .agents()
        .take(MAX_REPORTED_AGENTS)
        .map(|agent| {
            let mut providers = BTreeSet::new();
            let mut capabilities = Vec::new();
            if agent.spec.capabilities.len() > MAX_REPORTED_AGENT_CAPABILITIES {
                truncated = true;
            }
            for capability_id in agent
                .spec
                .capabilities
                .iter()
                .take(MAX_REPORTED_AGENT_CAPABILITIES)
            {
                let Some(capability) = catalog.capability(capability_id) else {
                    // Catalog validation already proved this reference. Keeping this defensive
                    // branch makes reporting incapable of turning a future loader regression into
                    // gateway authority or a panic.
                    truncated = true;
                    continue;
                };
                if !providers.contains(&capability.spec.provider)
                    && providers.len() == MAX_REPORTED_AGENT_PROVIDERS
                {
                    truncated = true;
                    continue;
                }
                providers.insert(capability.spec.provider.clone());
                if capability.spec.permissions.len() > MAX_REPORTED_PERMISSIONS {
                    truncated = true;
                }
                capabilities.push(ReportedAgentCapability {
                    id: capability_id.clone(),
                    provider: capability.spec.provider.clone(),
                    permissions: capability
                        .spec
                        .permissions
                        .iter()
                        .take(MAX_REPORTED_PERMISSIONS)
                        .cloned()
                        .map(|mut permission| {
                            permission.operation =
                                bounded_report_text(&permission.operation, &mut truncated);
                            permission.resource = permission
                                .resource
                                .as_deref()
                                .map(|resource| bounded_report_text(resource, &mut truncated));
                            permission
                        })
                        .collect(),
                });
            }
            for provider in &agent.spec.providers {
                if !providers.contains(provider) && providers.len() == MAX_REPORTED_AGENT_PROVIDERS
                {
                    truncated = true;
                    continue;
                }
                providers.insert(provider.clone());
            }
            ReportedAgent {
                id: agent
                    .metadata
                    .name
                    .parse()
                    .expect("catalog validation produces valid agent identifiers"),
                description: bounded_report_text(&agent.spec.description, &mut truncated),
                enabled: agent.spec.enabled,
                model_class: agent
                    .spec
                    .model_class
                    .as_deref()
                    .map(|class| bounded_report_text(class, &mut truncated)),
                providers: providers.into_iter().collect(),
                capabilities,
            }
        })
        .collect();
    AgentInventory { agents, truncated }
}

fn bounded_report_text(value: &str, truncated: &mut bool) -> String {
    if value.len() <= MAX_REPORTED_TEXT_BYTES {
        return value.to_owned();
    }
    *truncated = true;
    let mut end = MAX_REPORTED_TEXT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

async fn report_status(
    broker: config::ResolvedBroker,
    inventory: AgentInventory,
    mut reports: mpsc::Receiver<ModelUsageReport>,
) {
    let mut heartbeat = tokio::time::interval(STATUS_INVENTORY_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately once. The synchronous startup report already did that job.
    heartbeat.tick().await;
    loop {
        tokio::select! {
            report = reports.recv() => {
                let Some(mut report) = report else { break };
                while let Ok(next) = reports.try_recv() {
                    merge_usage(&mut report, next);
                }
                bound_usage_report(&mut report);
                let client = match BrokerClient::new(
                    &broker.socket_path,
                    broker.server_uid,
                    broker.frame,
                ) {
                    Ok(client) => client,
                    Err(_) => {
                        tracing::warn!(event = "gateway_usage_report_failed");
                        continue;
                    }
                };
                match timeout(STATUS_REPORT_TIMEOUT, client.publish_model_usage(report)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) | Err(_) => tracing::warn!(event = "gateway_usage_report_failed"),
                }
            }
            _ = heartbeat.tick() => {
                let client = match BrokerClient::new(
                    &broker.socket_path,
                    broker.server_uid,
                    broker.frame,
                ) {
                    Ok(client) => client,
                    Err(_) => {
                        tracing::warn!(event = "gateway_agent_inventory_report_failed");
                        continue;
                    }
                };
                match timeout(
                    STATUS_REPORT_TIMEOUT,
                    client.publish_agent_inventory(inventory.clone()),
                ).await {
                    Ok(Ok(())) => tracing::debug!(event = "gateway_agent_inventory_refreshed"),
                    Ok(Err(_)) | Err(_) => {
                        tracing::warn!(event = "gateway_agent_inventory_report_failed")
                    }
                }
            }
        }
    }
}

fn bound_usage_report(report: &mut ModelUsageReport) {
    report.model_calls = report
        .model_calls
        .min(dekopon_broker_protocol::MAX_REPORTED_MODEL_CALLS);
    report.input_unreported_calls = report.input_unreported_calls.min(report.model_calls);
    report.cached_input_unreported_calls =
        report.cached_input_unreported_calls.min(report.model_calls);
    report.output_unreported_calls = report.output_unreported_calls.min(report.model_calls);
    report.reasoning_unreported_calls = report.reasoning_unreported_calls.min(report.model_calls);
    report.total_unreported_calls = report.total_unreported_calls.min(report.model_calls);
    report.input_tokens = report
        .input_tokens
        .min(dekopon_broker_protocol::MAX_REPORTED_TOKENS);
    report.cached_input_tokens = report
        .cached_input_tokens
        .min(dekopon_broker_protocol::MAX_REPORTED_TOKENS);
    report.output_tokens = report
        .output_tokens
        .min(dekopon_broker_protocol::MAX_REPORTED_TOKENS);
    report.reasoning_output_tokens = report
        .reasoning_output_tokens
        .min(dekopon_broker_protocol::MAX_REPORTED_TOKENS);
    report.total_tokens = report
        .total_tokens
        .min(dekopon_broker_protocol::MAX_REPORTED_TOKENS);
}

fn merge_usage(total: &mut ModelUsageReport, next: ModelUsageReport) {
    total.model_calls = total.model_calls.saturating_add(next.model_calls);
    total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
    total.input_unreported_calls = total
        .input_unreported_calls
        .saturating_add(next.input_unreported_calls);
    total.cached_input_tokens = total
        .cached_input_tokens
        .saturating_add(next.cached_input_tokens);
    total.cached_input_unreported_calls = total
        .cached_input_unreported_calls
        .saturating_add(next.cached_input_unreported_calls);
    total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
    total.output_unreported_calls = total
        .output_unreported_calls
        .saturating_add(next.output_unreported_calls);
    total.reasoning_output_tokens = total
        .reasoning_output_tokens
        .saturating_add(next.reasoning_output_tokens);
    total.reasoning_unreported_calls = total
        .reasoning_unreported_calls
        .saturating_add(next.reasoning_unreported_calls);
    total.total_tokens = total.total_tokens.saturating_add(next.total_tokens);
    total.total_unreported_calls = total
        .total_unreported_calls
        .saturating_add(next.total_unreported_calls);
}

/// The routing loop: one message in, at most one session task out.
#[allow(clippy::too_many_arguments)]
async fn serve<F>(
    runner: Arc<SessionRunner>,
    routes: Arc<RoutingTable>,
    identities: Arc<BTreeMap<String, TransportIdentity>>,
    repliers: Arc<BTreeMap<String, Arc<dyn ChatReplier>>>,
    mut receiver: mpsc::Receiver<TransportEvent>,
    shutdown: F,
    grace: Duration,
) where
    F: Future<Output = ()> + Send,
{
    let mut sessions = JoinSet::new();
    tokio::pin!(shutdown);

    loop {
        // Reaped opportunistically rather than awaited: a finished session's task must not hold a
        // slot in the set while the loop is blocked waiting for the next message.
        while let Some(result) = sessions.try_join_next() {
            observe_session(result);
        }
        tokio::select! {
            () = &mut shutdown => break,
            event = receiver.recv() => {
                let Some(event) = event else { break };
                match event {
                    TransportEvent::Message(message) => {
                        dispatch(&runner, &routes, &identities, &repliers, &mut sessions, *message);
                    }
                    TransportEvent::SessionStopped(request) => {
                        stop_session(&runner, &mut sessions, request);
                    }
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
    repliers: &BTreeMap<String, Arc<dyn ChatReplier>>,
    sessions: &mut JoinSet<()>,
    message: InboundMessage,
) {
    let Some(route) = routes.route(&message.transport, &message.conversation) else {
        // Bots see ambient traffic. Silence is the correct answer, and debug level keeps a busy
        // channel from becoming the daemon's log volume.
        tracing::debug!(
            event = "gateway_message_ignored",
            transport = %message.transport,
            reason = "unrouted"
        );
        return;
    };
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
    if matches!(message.conversation, ConversationKind::Channel(_))
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
    let Some(replier) = repliers.get(&message.transport).cloned() else {
        tracing::error!(
            event = "gateway_message_ignored",
            transport = %message.transport,
            reason = "no-replier"
        );
        return;
    };
    sessions.spawn(session::run_session(
        Arc::clone(runner),
        route.clone(),
        message,
        replier,
    ));
}

fn stop_session(runner: &Arc<SessionRunner>, sessions: &mut JoinSet<()>, request: SessionStop) {
    let Some(reply) = runner.active_sessions.stop(&request) else {
        tracing::debug!(
            event = "gateway_session_stop_ignored",
            transport = %request.transport
        );
        return;
    };
    tracing::info!(
        event = "gateway_session_stop_requested",
        transport = %request.transport
    );
    sessions.spawn(async move {
        if let Err(error) = reply
            .replier
            .reply(reply.target, OutboundReply::text(STOPPED_REPLY))
            .await
        {
            tracing::error!(event = "gateway_reply_failed", category = error.category());
        }
    });
}

/// One reader task per transport, feeding the routing loop.
async fn read_transport(
    mut transport: Box<dyn ChatTransport>,
    sender: mpsc::Sender<TransportEvent>,
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
                return;
            }
        }
    }
}

fn build_transport(spec: &TransportConfig) -> Result<Box<dyn ChatTransport>, DekopondError> {
    let name = spec.name().to_owned();
    let build = || -> Result<Box<dyn ChatTransport>, TransportError> {
        Ok(match spec {
            TransportConfig::SlackSocketMode {
                name,
                app_token_env,
                bot_token_env,
                experience,
                activity,
                endpoint,
            } => Box::new(SlackTransport::new(
                name.clone(),
                endpoint
                    .clone()
                    .unwrap_or_else(|| config::SLACK_ENDPOINT.to_owned()),
                transport::read_credential(app_token_env)?,
                transport::read_credential(bot_token_env)?,
                *experience,
                *activity,
            )?),
            TransportConfig::DiscordGateway {
                name,
                bot_token_env,
                activity,
                endpoint,
            } => Box::new(DiscordTransport::new(
                name.clone(),
                endpoint
                    .clone()
                    .unwrap_or_else(|| config::DISCORD_ENDPOINT.to_owned()),
                transport::read_credential(bot_token_env)?,
                activity.mode,
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
            )?),
            TransportConfig::TelegramLongPoll {
                name,
                bot_token_env,
                activity,
                endpoint,
            } => Box::new(TelegramTransport::new(
                name.clone(),
                endpoint
                    .clone()
                    .unwrap_or_else(|| config::TELEGRAM_ENDPOINT.to_owned()),
                transport::read_credential(bot_token_env)?,
                activity.mode,
            )?),
            TransportConfig::Local { name, socket_path } => {
                Box::new(LocalTransport::new(name.clone(), socket_path.clone()))
            }
        })
    };
    build().map_err(|source| DekopondError::TransportConnect {
        transport: name,
        source,
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
    /// A named image generator could not resolve its model credential or client.
    #[error("configured image generator is unavailable")]
    ImageGenerator(#[source] ImageGeneratorStartupError),
    /// The configured broker did not answer a capability probe at startup.
    #[error("broker is not reachable; start dekopon-brokerd before the gateway")]
    BrokerProbe(#[source] dekopon_broker_protocol::ClientError),
    /// A transport could not authenticate or open its wakeup path.
    #[error("chat transport {transport} could not connect")]
    TransportConnect {
        /// Configured transport name.
        transport: String,
        #[source]
        source: TransportError,
    },
}

#[cfg(test)]
mod tests;
