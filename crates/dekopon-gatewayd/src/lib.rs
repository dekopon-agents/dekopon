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
mod follow;
mod gateway;
mod jobs;
mod memory;
use memory::journal;
mod meter_restore;
mod metering;
mod probe;
mod progress;
mod proxy;
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
    time::Duration,
};

use dekopon_broker_protocol::BrokerClient;
use dekopon_config::LocalCatalog;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinSet};

pub use config::{
    CONFIG_API_VERSION, ConfigApiVersion, ConfigError, ConfigProblem, ConversationMatchConfig,
    DEFAULT_FORGET_AFTER, DEFAULT_STOP_WORDS, GatewaydConfig, HARD_MAX_CONFIG_BYTES,
    IndicatorSurface, JournalConfig, KeepAliveConfig, LivenessConfig, LivenessMode,
    LivenessOverride, LivenessSettings, MemoryConfig, MemoryPolicy, MemoryScope, MemoryWindow,
    ProgressSurface, ReactionConfig, RecallSource, ResolvedConfig, ResolvedJournal, ResolvedRoute,
    ResolvedTelemetry, SeenReaction, SlackExperience, TelemetryConfig, TemplateOverrides,
    TransportConfig, WorkingReaction,
};
pub use probe::{NotProbed, ProbeError, ProbeProblem, ProbeScope};
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
        ConfiguredModels, ModelCache, ModelCredentialError, SessionGate, SessionRunner,
        model_bearer_token,
    },
    transport::{
        AssetFetcher, CancelRequest, ChatDriver, ChatTransport, ThreadOwnership, TransportEvent,
        discord::{DiscordTransport, MessageContent},
        github::GithubTransport,
        local::LocalTransport,
        slack::SlackTransport,
        telegram::TelegramTransport,
        whatsapp::WhatsappTransport,
    },
};

pub const OTEL_TRACE_FILTER: &str = "dekopon_gatewayd=trace,dekopon_agent=trace,dekopon_process=trace,dekopon_shell=trace,dekopon_model=trace,gateway=debug,prompt=debug,model=debug,asset=debug,shell=debug,job=debug,broker=debug,provider=debug,http=debug,credential=debug,memory=debug,telemetry=debug,hyper=off,h2=off,reqwest=off,tungstenite=off,tokio_tungstenite=off";

const INBOUND_BUFFER: usize = 64;
const ASSET_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[must_use]
pub fn current_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub async fn telemetry_settings(
    config_path: impl AsRef<Path>,
    uid: u32,
) -> Result<Option<ResolvedTelemetry>, GatewaydError> {
    Ok(config::load(config_path, uid).await?.telemetry)
}

async fn boot_candidate(config_path: &Path) -> Result<(Candidate, Prepared), GatewaydError> {
    let (candidate, report) = load_candidate(config_path, None, config::LoadMode::Boot, None).await;
    let Some((candidate, prepared)) = candidate.and_then(|mut candidate| {
        candidate
            .prepared
            .take()
            .map(|prepared| (candidate, prepared))
    }) else {
        return Err(GatewaydError::from_problems(report.problems));
    };
    if !candidate
        .sources
        .still_current(config_path, &candidate.config.catalog_path, current_uid())
        .await?
    {
        return Err(config::ConfigError::ChangedDuringLoad.into());
    }
    Ok((candidate, prepared))
}

#[expect(
    clippy::too_many_lines,
    reason = "gateway boot owns resident transports and reload owners until shutdown"
)]
pub async fn run<F>(config_path: impl AsRef<Path>, shutdown: F) -> Result<(), GatewaydError>
where
    F: Future<Output = ()> + Send,
{
    let (
        candidate,
        Prepared {
            transports: built_transports,
            generation,
        },
    ) = boot_candidate(config_path.as_ref()).await?;
    let config = &candidate.config;

    let broker_client = BrokerClient::new(
        &config.broker.socket_path,
        config.broker.server_uid,
        config.broker.frame,
    )
    .map_err(GatewaydError::BrokerProbe)?;
    let capabilities = broker_client
        .capabilities()
        .await
        .map_err(GatewaydError::BrokerProbe)?;
    let (broker_digest, outcome) = broker_client
        .observe_generation()
        .await
        .map_err(GatewaydError::BrokerProbe)?;
    if outcome != dekopon_broker_protocol::GenerationOutcome::Active {
        return Err(GatewaydError::BrokerGeneration);
    }
    tracing::info!(
        event = "gateway_broker_ready",
        capability.count = capabilities.len(),
        "gateway.broker.ready"
    );
    let mut drivers: BTreeMap<String, Arc<dyn ChatDriver>> = BTreeMap::new();
    let mut asset_fetchers: HashMap<String, Arc<dyn AssetFetcher>> = HashMap::new();
    let mut thread_ownership: HashMap<String, Arc<dyn ThreadOwnership>> = HashMap::new();
    let (sender, receiver) = mpsc::channel::<TransportEvent>(INBOUND_BUFFER);
    let mut readers = JoinSet::new();
    for transport in built_transports {
        let transport = register_transport(
            transport,
            &mut drivers,
            &mut asset_fetchers,
            &mut thread_ownership,
        );
        readers.spawn(read_transport(Box::new(transport), sender.clone()));
    }

    let journal = open_journal(config.journal.clone(), &config.memory).await?;
    let journal_dir = config.journal.as_ref().map(|journal| journal.dir.clone());
    let cache_keys = tokio::task::spawn_blocking(move || match journal_dir {
        Some(dir) => cache_key::CacheKeySecret::load_or_create(&dir),
        None => cache_key::CacheKeySecret::generate(),
    })
    .await
    .map_err(GatewaydError::TransportTask)??;
    if journal.is_some() {
        for route in &config.routes {
            if let (Some(window), Some(policy)) =
                (route.memory.window(), route.memory_policy.as_ref())
            {
                memory::generation::report_policy(policy, window.recall);
            }
        }
    }
    let wakes = match config.wakes.clone() {
        Some(wakes) => Some(Arc::new(
            tokio::task::spawn_blocking(move || wake::WakeStore::open(&wakes))
                .await
                .map_err(GatewaydError::TransportTask)??,
        )),
        None => None,
    };
    let configured = Arc::new(ConfiguredModels::default());
    let metering = Arc::new(metering::build(
        &config.metering,
        dekopon_model_token_governor::Metering::system_clock(),
    ));
    let assets = Arc::new(AssetStore::with_retention(
        config.sessions.max_conversations,
        ASSET_IDLE_TIMEOUT,
        config.sessions.asset_retention_bytes,
    ));
    let runner = Arc::new(SessionRunner {
        pending_notices: Default::default(),
        sealed_conversations: Default::default(),
        broker: config.broker.clone(),
        models: Arc::new(ModelCache::new(
            Arc::clone(&configured) as Arc<dyn session::ModelFactory>
        )),
        gate: SessionGate::new(config.sessions.max_concurrent),
        reply_on_busy: config.sessions.reply_on_busy,
        conversations: ConversationStore::with_cache_keys(
            config.sessions.max_conversations,
            config.memory.arena.max_transient_bytes.get() as usize,
            cache_keys,
        ),
        follow: load_follow_table(&journal, config.routes.iter().any(|route| route.follow)).await?,
        journal,
        assets: Arc::clone(&assets),
        asset_fetchers,
        thread_ownership,
        wakes,
        jobs: Arc::new(jobs::Jobs::new(config.sessions.max_jobs)),
        metering: Arc::clone(&metering),
    });
    let (mut owned_tasks, boot_proxy) = start_metering_tasks(config, &configured, &metering)?;

    tracing::info!(
        event = "gateway_started",
        transport.count = config.transports.len(),
        route.count = candidate.routes.len(),
        "gateway.started"
    );

    let mut terminal = Ok(());
    let stopped = async {
        terminal = supervise_transports(&mut readers, shutdown).await;
        readers.abort_all();
    };
    let shutdown_grace = config.shutdown_grace;
    let collector = collection::Collector::new(&config.transports, config.sessions.max_concurrent);
    let proxy_listener = config.proxy.clone();
    let gateway = gateway::Gateway::serving(
        runner,
        generation,
        Arc::new(BTreeMap::new()),
        Arc::new(drivers),
    )
    .with_reload_owners(
        config_path.as_ref().to_path_buf(),
        candidate,
        broker_digest,
        gateway::ResidentModels {
            credentials: configured,
            metering,
            proxy: boot_proxy,
        },
    )?;
    if let Some(resolved) = &proxy_listener {
        proxy::listen(resolved, gateway.proxy_router(), &mut owned_tasks)
            .await
            .map_err(GatewaydError::Proxy)?;
    }
    let outcome = gateway
        .serve(receiver, stopped, shutdown_grace, collector)
        .await;
    readers.abort_all();
    owned_tasks.shutdown().await;
    assets.shutdown().await;
    while let Some(result) = readers.join_next().await {
        match result {
            Ok(Err(problem)) => tracing::warn!(
                event = "gateway_transport_stopped",
                transport = %problem.transport,
                category = problem.source.category(),
                "gateway.transport.stopped"
            ),
            Err(error) if !error.is_cancelled() => tracing::error!(
                event = "gateway_transport_task_failed", error = %error,
                "gateway.transport.task.failed"
            ),
            Ok(Ok(())) | Err(_) => {}
        }
    }
    drop(sender);
    if let Err(error) = terminal {
        tracing::error!(
            event = "gateway_stopped",
            reason = "transport-failed",
            "gateway.stopped"
        );
        return Err(error);
    }
    match outcome {
        ServeOutcome::Shutdown => {
            tracing::info!(
                event = "gateway_stopped",
                reason = "shutdown",
                "gateway.stopped"
            );
            Ok(())
        }
        // Exiting successfully here would let a gateway that lost every workspace to a revoked
        // token look like a clean run to whatever supervises it, so this is reported as a failure
        // instead.
        ServeOutcome::TransportsLost => {
            tracing::error!(
                event = "gateway_stopped",
                reason = "transports-lost",
                "gateway.stopped"
            );
            Err(GatewaydError::TransportsLost)
        }
    }
}

async fn open_journal(
    config: Option<config::ResolvedJournal>,
    memory: &config::ResolvedSettings,
) -> Result<Option<Arc<journal::Journal>>, GatewaydError> {
    let Some(config) = config else {
        return Ok(None);
    };
    let max_age = std::time::Duration::from_millis(memory.retention.max_age_ms.get());
    let agent_ages = memory
        .agents
        .iter()
        .map(|(id, agent)| (id.as_str().to_owned(), agent.max_age))
        .collect();
    let agent_bytes = memory
        .agents
        .iter()
        .map(|(id, agent)| (id.as_str().to_owned(), agent.max_bytes.get()))
        .collect();
    let max_bytes_per_agent = memory.retention.max_bytes_per_agent.get();
    let journal = tokio::task::spawn_blocking(move || {
        let mut journal = journal::Journal::open_with_age(
            &config.dir,
            max_age,
            config.retention,
            config.seal_retention,
        )?;
        journal.agent_ages = agent_ages;
        journal.agent_bytes = agent_bytes;
        journal.max_bytes_per_agent = max_bytes_per_agent;
        journal.enforce_caps()?;
        Ok::<_, journal::JournalError>(journal)
    })
    .await
    .map_err(GatewaydError::TransportTask)?
    .map_err(|error| GatewaydError::Journal {
        kind: match error {
            journal::JournalError::Io { kind } => kind,
            journal::JournalError::Corrupt => std::io::ErrorKind::InvalidData,
        },
    })?;
    Ok(Some(Arc::new(journal)))
}

async fn load_follow_table(
    journal: &Option<Arc<journal::Journal>>,
    enabled: bool,
) -> Result<Option<Arc<follow::FollowTable>>, GatewaydError> {
    if !enabled {
        return Ok(None);
    }
    let journal = Arc::clone(journal.as_ref().ok_or(GatewaydError::Journal {
        kind: std::io::ErrorKind::InvalidInput,
    })?);
    let table = tokio::task::spawn_blocking(move || follow::FollowTable::load(journal))
        .await
        .map_err(GatewaydError::TransportTask)?
        .map_err(|error| GatewaydError::Journal {
            kind: match error {
                journal::JournalError::Io { kind } => kind,
                journal::JournalError::Corrupt => std::io::ErrorKind::InvalidData,
            },
        })?;
    Ok(Some(Arc::new(table)))
}

fn start_metering_tasks(
    config: &ResolvedConfig,
    configured: &ConfiguredModels,
    metering: &Arc<dekopon_model_token_governor::Metering>,
) -> Result<(JoinSet<()>, Option<Arc<dekopon_model_proxy::ModelProxy>>), GatewaydError> {
    let mut owned_tasks = JoinSet::new();
    let boot_proxy = config
        .proxy
        .as_ref()
        .map(|resolved| proxy::build(resolved, &config.models, configured, metering))
        .transpose()
        .map_err(GatewaydError::Proxy)?;
    meter_restore::spawn(
        metering,
        config.metering.restore.clone(),
        config
            .telemetry
            .as_ref()
            .and_then(|telemetry| telemetry.settings.ca_certificate().map(<[u8]>::to_vec)),
        &mut owned_tasks,
    );
    Ok((owned_tasks, boot_proxy))
}

#[derive(Debug, Default)]
pub struct CheckReport {
    pub problems: Vec<GatewaydError>,
    pub warnings: Vec<CheckWarning>,
    pub(crate) raw: config::RawCandidate,
}

#[derive(Debug, Error)]
pub enum CheckWarning {
    #[error("baseline routes did not bind, so every model is probed")]
    BaselineRoutes {
        #[source]
        source: Box<GatewaydError>,
    },
    #[error("transport {transport} reads {variable} from the environment at boot")]
    TransportCredential { transport: String, variable: String },
    #[error("model {model} reads {variable} from the environment at boot")]
    ModelCredential { model: String, variable: String },
    #[error("persistent memory has no journal: window-only, memory tools unavailable")]
    MemoryWithoutJournal,
    #[error("{stage} not checked: a stage it needs failed")]
    Skipped { stage: Stage },
    #[error("model {model} not probed: {reason}")]
    NotProbed { model: String, reason: NotProbed },
    #[error("{} did not load, so every model is probed", .path.display())]
    Baseline {
        path: std::path::PathBuf,
        #[source]
        source: ConfigError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    Catalog,
    Routes,
    Credentials,
    Probe,
}

impl std::fmt::Display for Stage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Catalog => "agent catalog",
            Self::Routes => "routes",
            Self::Credentials => "credentials",
            Self::Probe => "vendor probe",
        })
    }
}

/// Runs the validation `run` runs, then stops: the broker socket is never contacted, no model login
/// or journal is read, and a credential variable is read only to probe a model's vendor.
pub async fn check(
    config_path: impl AsRef<Path>,
    catalog_path: Option<&Path>,
    probe: Option<&ProbeScope>,
) -> CheckReport {
    load_candidate(
        config_path.as_ref(),
        catalog_path,
        config::LoadMode::Check,
        probe.map(|scope| (scope, &probe::Vendor::LIVE)),
    )
    .await
    .1
}

pub(crate) struct Candidate {
    pub(crate) sources: config::GatewaySources,
    pub(crate) config: ResolvedConfig,
    pub(crate) routes: RoutingTable,
    pub(crate) prepared: Option<Prepared>,
}

/// Every stage whose inputs loaded runs, so one report names every problem; a stage whose inputs
/// did not is reported as skipped. `Check` reads no credential and builds no transport.
pub(crate) async fn load_candidate(
    config_path: &Path,
    catalog_path: Option<&Path>,
    mode: config::LoadMode,
    probe: Option<(&ProbeScope, &probe::Vendor)>,
) -> (Option<Candidate>, CheckReport) {
    load_candidate_against(config_path, catalog_path, mode, probe, None).await
}

pub(crate) async fn load_candidate_against(
    config_path: &Path,
    catalog_path: Option<&Path>,
    mode: config::LoadMode,
    probe: Option<(&ProbeScope, &probe::Vendor)>,
    active: Option<(&ResolvedConfig, &config::GatewaySources, &RoutingTable)>,
) -> (Option<Candidate>, CheckReport) {
    let mut report = CheckReport::default();
    let before_config = config::source_snapshot(config_path, None, current_uid()).await;
    let (config, refusals) = config::load_reporting(config_path, current_uid(), mode).await;
    report
        .problems
        .extend(refusals.into_iter().map(GatewaydError::from));
    let catalog_path = catalog_path.or(config.as_ref().map(|config| config.catalog_path.as_path()));
    let catalog = match catalog_path {
        Some(path) => match LocalCatalog::load(path) {
            Ok(catalog) => Some(catalog),
            Err(error) => {
                report.problems.push(GatewaydError::Catalog(error));
                None
            }
        },
        None => {
            report.skipped(Stage::Catalog);
            None
        }
    };
    let raw_catalog = match (&catalog, catalog_path) {
        (Some(catalog), _) => {
            Some(config::source_snapshot(catalog.source(), Some(catalog), current_uid()).await)
        }
        (None, Some(path)) => Some(config::source_snapshot(path, None, current_uid()).await),
        (None, None) => None,
    };
    report.raw = config::RawCandidate::of(
        before_config.as_ref(),
        raw_catalog.as_ref().map(Result::as_ref),
    );
    let routes = match (&config, &catalog) {
        (Some(config), Some(catalog)) => match RoutingTable::bind(config, catalog) {
            Ok(routes) => Some(routes),
            Err(error) => {
                report.problems.push(error.into());
                None
            }
        },
        (None, _) | (_, None) => {
            report.skipped(Stage::Routes);
            None
        }
    };
    let prepared = match (&config, &routes) {
        (Some(config), Some(routes)) => {
            if mode == config::LoadMode::Check
                && config.journal.is_none()
                && routes.has_persistent_memory()
            {
                report.warnings.push(CheckWarning::MemoryWithoutJournal);
            }
            if active.is_none() {
                credentials(config, routes, mode, &mut report)
            } else {
                let problems = model_credential_problems(routes.bound_models(), model_bearer_token);
                if !problems.is_empty() {
                    report.problems.push(GatewaydError::Startup { problems });
                }
                None
            }
        }
        (None, _) | (_, None) => {
            report.skipped(Stage::Credentials);
            None
        }
    };
    let reload_scope = ProbeScope::Every;
    let probe = probe.or_else(|| active.map(|_| (&reload_scope, &probe::Vendor::LIVE)));
    match (probe, &config) {
        (Some((scope, vendor)), Some(config)) => {
            probe::probe(
                config,
                routes.as_ref(),
                scope,
                active.map(|(config, _, routes)| (config, routes)),
                vendor,
                &mut report,
            )
            .await;
        }
        (Some(_), None) => report.skipped(Stage::Probe),
        (None, _) => {}
    }
    let candidate = match (config, routes, catalog) {
        (Some(config), Some(routes), Some(catalog)) if report.problems.is_empty() => {
            let after_config = config::source_snapshot(config_path, None, current_uid()).await;
            let after_catalog =
                config::source_snapshot(catalog.source(), Some(&catalog), current_uid()).await;
            match (before_config, after_config, after_catalog) {
                (Ok(before), Ok(after), Ok(catalog_after)) if before == after => {
                    let sources = config::GatewaySources {
                        config: after,
                        catalog: catalog_after,
                    };
                    let changes = active.map(|(_, old, _)| old.changes(&sources)).transpose();
                    match changes {
                        Ok(Some(changes)) if !changes.restart_required.is_empty() => {
                            report.problems.push(
                                config::ConfigError::RestartRequired {
                                    categories: changes.restart_required,
                                }
                                .into(),
                            );
                            None
                        }
                        Ok(_) => Some(Candidate {
                            sources,
                            config,
                            routes,
                            prepared,
                        }),
                        Err(error) => {
                            report.problems.push(error.into());
                            None
                        }
                    }
                }
                (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                    report.problems.push(error.into());
                    None
                }
                _ => {
                    report
                        .problems
                        .push(config::ConfigError::ChangedDuringLoad.into());
                    None
                }
            }
        }
        _ => None,
    };
    (candidate, report)
}

impl CheckReport {
    fn skipped(&mut self, stage: Stage) {
        self.warnings.push(CheckWarning::Skipped { stage });
    }
}

fn credentials(
    config: &ResolvedConfig,
    routes: &RoutingTable,
    mode: config::LoadMode,
    report: &mut CheckReport,
) -> Option<Prepared> {
    match mode {
        config::LoadMode::Boot => match prepare(config, routes) {
            Ok(prepared) => return Some(prepared),
            Err(error) => report.problems.push(error),
        },
        config::LoadMode::Check => {
            for spec in &config.transports {
                report
                    .warnings
                    .extend(
                        transport_credential_variables(spec)
                            .into_iter()
                            .map(|variable| CheckWarning::TransportCredential {
                                transport: spec.name().to_owned(),
                                variable: variable.to_owned(),
                            }),
                    );
            }
            for model in routes.bound_models() {
                let mut required = None;
                let missing = session::model_bearer_token_with(model, |variable| {
                    required = Some(variable.to_owned());
                    None
                })
                .is_err();
                if let (true, Some(variable)) = (missing, required) {
                    report.warnings.push(CheckWarning::ModelCredential {
                        model: model.name().to_owned(),
                        variable,
                    });
                }
            }
        }
    }
    None
}

fn transport_credential_variables(spec: &TransportConfig) -> Vec<&str> {
    match spec {
        TransportConfig::SlackSocketMode {
            app_token_env,
            bot_token_env,
            ..
        } => vec![app_token_env, bot_token_env],
        TransportConfig::DiscordGateway { bot_token_env, .. }
        | TransportConfig::TelegramLongPoll { bot_token_env, .. } => vec![bot_token_env],
        TransportConfig::WhatsappCloudApi {
            app_secret_env,
            verify_token_env,
            access_token_env,
            ..
        } => vec![app_secret_env, verify_token_env, access_token_env],
        TransportConfig::GithubWebhook { secret_env, .. } => vec![secret_env],
        TransportConfig::Local { .. } => Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServeOutcome {
    Shutdown,
    TransportsLost,
}

#[cfg(test)]
pub(crate) use tests::{dispatch, serve};

fn observe_session(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::error!(
            event = "gateway_session_task_failed",
            "gateway.session.task.failed"
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
        .admit_probe(("wake".to_owned(), tick.id().to_string()))
    else {
        tracing::info!(
            event = "gateway_wake_tick_skipped",
            wake.id = %tick.id(),
            reason = "busy",
            "gateway.wake.tick.skipped"
        );
        return;
    };
    let limits = routes
        .route_for_anchor(tick.anchor())
        .map(|route| dekopon_process::Limits {
            max_capability_calls: route.limits.max_capability_calls,
            timeout: route.script_timeout,
            ..dekopon_process::Limits::default()
        });
    let job_limits = routes.route_for_anchor(tick.anchor()).and_then(|route| {
        route.job_timeout.map(|timeout| dekopon_process::Limits {
            max_capability_calls: route.limits.max_capability_calls,
            timeout,
            ..dekopon_process::Limits::default()
        })
    });
    let jobs = Arc::clone(&runner.jobs);
    let broker = runner.broker.clone();
    let runtime = tokio::runtime::Handle::current();
    ticks.spawn_blocking(move || {
        let _admission = admission;
        wake::run_tick(
            tick,
            &store,
            &broker,
            &runtime,
            limits,
            job_limits.map(|limits| (jobs, limits)),
        )
    });
}

fn cancel_session(runner: &Arc<SessionRunner>, request: &CancelRequest) {
    runner.jobs.cancel_owner(request);
    match runner.gate.cancel(request).running.ignored_reason() {
        None => tracing::info!(
            event = "gateway_session_stop_requested",
            transport = %request.transport,
            via = via_label(request.via),
            "gateway.session.stop.requested"
        ),
        // Ignored here since only the subject that started a session may stop it, and a session
        // that already ended has nothing left to stop.
        Some(reason) => tracing::debug!(
            event = "gateway_session_stop_ignored",
            transport = %request.transport,
            reason,
            "gateway.session.stop.ignored"
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

fn register_transport(
    transport: Box<dyn ChatTransport>,
    drivers: &mut BTreeMap<String, Arc<dyn ChatDriver>>,
    asset_fetchers: &mut HashMap<String, Arc<dyn AssetFetcher>>,
    thread_ownership: &mut HashMap<String, Arc<dyn ThreadOwnership>>,
) -> transport::recovery::RecoveringTransport {
    let transport = transport::recovery::RecoveringTransport::new(transport);
    let name = transport.name().to_owned();
    drivers.insert(name.clone(), transport.driver());
    if let Some((sibling, driver)) = transport.sibling_driver() {
        drivers.insert(sibling, driver);
    }
    if let Some((sibling, fetcher)) = transport.sibling_fetcher() {
        asset_fetchers.insert(sibling, fetcher);
    }
    if let Some(fetcher) = transport.asset_fetcher() {
        asset_fetchers.insert(name.clone(), fetcher);
    }
    if let Some(ownership) = transport.thread_ownership() {
        thread_ownership.insert(name, ownership);
    }
    transport
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
) -> Result<(), GatewaydError>
where
    F: Future<Output = ()> + Send,
{
    tokio::select! {
        biased;
        () = shutdown => Ok(()),
        result = readers.join_next() => match result {
            Some(Ok(Err(problem))) => Err(GatewaydError::TransportConnect { problems: vec![problem] }),
            Some(Err(source)) => Err(GatewaydError::TransportTask(source)),
            Some(Ok(Ok(()))) | None => Err(GatewaydError::TransportsLost),
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

fn prepare(config: &ResolvedConfig, routes: &RoutingTable) -> Result<Prepared, GatewaydError> {
    let mut problems = model_credential_problems(routes.bound_models(), model_bearer_token);
    let generation = gateway::CurrentGeneration::boot(
        Arc::new(routes.clone()),
        Arc::new(config.stop_words.clone()),
    );
    let whatsapp_binds = config
        .transports
        .iter()
        .filter_map(|spec| match spec {
            TransportConfig::WhatsappCloudApi { bind, .. } => Some(*bind),
            TransportConfig::GithubWebhook { .. }
            | TransportConfig::SlackSocketMode { .. }
            | TransportConfig::DiscordGateway { .. }
            | TransportConfig::TelegramLongPoll { .. }
            | TransportConfig::Local { .. } => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let mut github_mounts = HashMap::new();
    for spec in &config.transports {
        if let TransportConfig::GithubWebhook {
            name,
            bind,
            secret_env,
        } = spec
            && whatsapp_binds.contains(bind)
        {
            match transport::read_credential(secret_env) {
                Ok(secret) => {
                    github_mounts.insert(*bind, (name.clone(), secret));
                }
                Err(source) => problems.push(StartupProblem::Transport {
                    transport: name.clone(),
                    source,
                }),
            }
        }
    }
    let mut transports = Vec::with_capacity(config.transports.len());
    for spec in &config.transports {
        if matches!(spec, TransportConfig::GithubWebhook { bind, .. } if whatsapp_binds.contains(bind))
        {
            continue;
        }
        match build_transport(spec, config, &generation, &mut github_mounts) {
            Ok(transport) => transports.push(transport),
            Err(source) => problems.push(StartupProblem::Transport {
                transport: spec.name().to_owned(),
                source,
            }),
        }
    }
    if problems.is_empty() {
        Ok(Prepared {
            transports,
            generation,
        })
    } else {
        Err(GatewaydError::Startup { problems })
    }
}

pub(crate) struct Prepared {
    transports: Vec<Box<dyn ChatTransport>>,
    generation: gateway::CurrentGeneration,
}

fn build_transport(
    spec: &TransportConfig,
    config: &ResolvedConfig,
    generation: &gateway::CurrentGeneration,
    github_mounts: &mut HashMap<std::net::SocketAddr, (String, String)>,
) -> Result<Box<dyn ChatTransport>, TransportError> {
    let surface = |liveness: &LivenessConfig| {
        let mut settings = liveness.settings();
        if config
            .routes
            .iter()
            .filter(|route| route.transport == spec.name())
            .flat_map(|route| route.indicators.values())
            .any(|resolved| resolved.settings.mode == LivenessMode::Native)
        {
            settings.mode = LivenessMode::Native;
        }
        settings
    };
    Ok(match spec {
        TransportConfig::GithubWebhook {
            name,
            secret_env,
            bind,
        } => Box::new(GithubTransport::new(
            name.clone(),
            *bind,
            transport::read_credential(secret_env)?,
            generation.clone(),
            None,
        )),
        TransportConfig::SlackSocketMode {
            name,
            app_token_env,
            bot_token_env,
            experience,
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
        )?),
        TransportConfig::DiscordGateway {
            name,
            bot_token_env,
            message_content,
            liveness,
            endpoint,
        } => Box::new(DiscordTransport::new(
            name.clone(),
            endpoint
                .clone()
                .unwrap_or_else(|| config::DISCORD_ENDPOINT.to_owned()),
            transport::read_credential(bot_token_env)?,
            if *message_content {
                MessageContent::Requested
            } else {
                MessageContent::Withheld
            },
            surface(liveness),
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
        } => {
            let whatsapp = WhatsappTransport::new(
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
                surface(liveness),
            )?;
            match github_mounts.remove(bind) {
                Some((name, secret)) => Box::new(GithubTransport::new(
                    name,
                    *bind,
                    secret,
                    generation.clone(),
                    Some(whatsapp),
                )),
                None => Box::new(whatsapp),
            }
        }
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
            surface(liveness),
        )?),
        TransportConfig::Local {
            name,
            socket_path,
            liveness,
        } => Box::new(LocalTransport::new(
            name.clone(),
            socket_path.clone(),
            surface(liveness),
        )),
    })
}

#[derive(Debug, Error)]
pub enum GatewaydError {
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
    #[error("broker generation was not active")]
    BrokerGeneration,
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
    CacheKeySecret(#[from] cache_key::CacheKeySecretError),
    #[error(transparent)]
    WakeStore(#[from] WakeStoreError),
    #[error("guest model proxy could not start")]
    Proxy(#[source] proxy::ProxyStartError),
    #[error(transparent)]
    Probe(Box<ProbeProblem>),
    #[error("{}", render_problems(.problems))]
    Problems { problems: Vec<GatewaydError> },
}

impl GatewaydError {
    fn from_problems(mut problems: Vec<Self>) -> Self {
        match (problems.pop(), problems.is_empty()) {
            (Some(only), true) => only,
            (Some(last), false) => {
                problems.push(last);
                Self::Problems { problems }
            }
            (None, _) => Self::Problems { problems },
        }
    }
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
