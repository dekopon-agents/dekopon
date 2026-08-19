//! The unprivileged Dekopon chat gateway and agent daemon.
//!
//! `dekopond` connects to chat services, waits efficiently for a wakeup, routes each authenticated
//! message to a named agent from the catalog, runs one bounded model session whose only tool is the
//! sandboxed shell, and replies with the answer.
//!
//! # Authority
//!
//! It has none. It holds chat bot credentials and model credentials — the things it needs to hear a
//! question and to ask a model — and it never holds a provider credential, a policy, or an
//! authorization. Every effect a session drives is submitted to `dekopon-brokerd` as an *attested*
//! proposal naming the sender's canonical subject, and the broker alone maps that subject to a
//! principal, decides what it may do, and executes it. The daemon's dependency set excludes every
//! privileged broker crate for the same reason `dekopon-run`'s does, and CI enforces it.
//!
//! Everything arriving from a chat service is untrusted, including the agent's own standing orders
//! from the catalog: neither can assert identity, name a principal, or widen a grant.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod asset;
mod cache_key;
mod config;
mod conversation;
mod routes;
mod session;
mod transport;

pub mod cli;

use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    path::Path,
    sync::Arc,
    time::Duration,
};

use dekopon_broker_protocol::BrokerClient;
use dekopon_config::LocalCatalog;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinSet, time::timeout};

pub use config::{
    CONFIG_API_VERSION, ConfigApiVersion, ConfigError, ConversationConfig, ConversationPolicy,
    ConversationWindow, DekopondConfig, HARD_MAX_CONFIG_BYTES, ResolvedConfig, ResolvedRoute,
    ResolvedTelemetry, SocketDiscovery, TelemetryConfig,
};
pub use routes::RouteError;
pub use session::SessionError;
pub use transport::TransportError;

use crate::{
    asset::AssetStore,
    config::TransportConfig,
    conversation::ConversationStore,
    routes::RoutingTable,
    session::{ConfiguredModels, SessionGate, SessionRunner},
    transport::{
        AssetFetcher, ChatReplier, ChatTransport, ConversationKind, InboundMessage,
        TransportIdentity, local::LocalTransport, slack::SlackTransport,
        telegram::TelegramTransport,
    },
};

/// Inbound messages buffered between the transport readers and the routing loop.
///
/// Bounded, so a chat service having a busy minute applies backpressure to the reader rather than
/// growing a queue the daemon can never work through. Admission control refuses the overflow with
/// a sentence, which is a better answer than an unbounded backlog.
const INBOUND_BUFFER: usize = 64;

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

    // One probe before anything connects, so "the broker is not running" is a startup failure with
    // a clear message rather than every session failing identically an hour later.
    let capabilities = BrokerClient::new(
        &config.broker.socket_path,
        config.broker.server_uid,
        config.broker.frame,
    )
    .map_err(DekopondError::BrokerProbe)?
    .capabilities()
    .await
    .map_err(DekopondError::BrokerProbe)?;
    tracing::info!(
        event = "gateway_broker_ready",
        capability.count = capabilities.len()
    );

    let mut transports = Vec::with_capacity(config.transports.len());
    let mut identities = BTreeMap::new();
    let mut repliers: BTreeMap<String, Arc<dyn ChatReplier>> = BTreeMap::new();
    let mut asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>> = HashMap::new();
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
        transports.push(transport);
    }

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
    });

    let (sender, receiver) = mpsc::channel::<InboundMessage>(INBOUND_BUFFER);
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

    tracing::info!(event = "gateway_stopped");
    Ok(())
}

/// The routing loop: one message in, at most one session task out.
#[allow(clippy::too_many_arguments)]
async fn serve<F>(
    runner: Arc<SessionRunner>,
    routes: Arc<RoutingTable>,
    identities: Arc<BTreeMap<String, TransportIdentity>>,
    repliers: Arc<BTreeMap<String, Arc<dyn ChatReplier>>>,
    mut receiver: mpsc::Receiver<InboundMessage>,
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
            message = receiver.recv() => {
                let Some(message) = message else { break };
                dispatch(&runner, &routes, &identities, &repliers, &mut sessions, message);
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
    // A channel route that fired on every message would be noise and cost, so a shared
    // conversation requires the bot to be addressed. A direct message is addressed by definition.
    if matches!(message.conversation, ConversationKind::Channel(_))
        && !identities
            .get(&message.transport)
            .is_some_and(|identity| identity.is_addressed(&message.text))
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

/// One reader task per transport, feeding the routing loop.
async fn read_transport(
    mut transport: Box<dyn ChatTransport>,
    sender: mpsc::Sender<InboundMessage>,
) {
    loop {
        match transport.next().await {
            Ok(message) => {
                if sender.send(message).await.is_err() {
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
                endpoint,
            } => Box::new(SlackTransport::new(
                name.clone(),
                endpoint
                    .clone()
                    .unwrap_or_else(|| config::SLACK_ENDPOINT.to_owned()),
                transport::read_credential(app_token_env)?,
                transport::read_credential(bot_token_env)?,
            )?),
            TransportConfig::TelegramLongPoll {
                name,
                bot_token_env,
                endpoint,
            } => Box::new(TelegramTransport::new(
                name.clone(),
                endpoint
                    .clone()
                    .unwrap_or_else(|| config::TELEGRAM_ENDPOINT.to_owned()),
                transport::read_credential(bot_token_env)?,
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
