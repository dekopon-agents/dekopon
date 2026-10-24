//! The server derives caller identity exclusively from Unix peer credentials and trusted
//! owner-controlled configuration; wire payloads are always untrusted invocation proposals.

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
mod assets;
pub mod capabilities;

const BROKER_PRINCIPAL: &str = "dekopon-broker";
const RECONCILIATION_OWNER: &str = "dekopon-gatewayd";
mod config;
mod credentials;
mod github_app;
mod lock_watch;
#[cfg(test)]
mod private_read_witness;
mod provider_manager;
mod reaper;
mod refresh_scheduler;
mod secrets;
mod server;
mod socket;

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use config::{LoadMode, Resident};
use dekopon_broker::{
    AuditLog, Broker, ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory,
    Leniency, PolicyBuildError, PolicyEngine, PolicyWorld, TraceOnlyAuditLog,
};
use dekopon_broker_host::BrokerProviderRegistry;
use dekopon_broker_protocol::ResponseEnvelope;
use dekopon_capability::EffectKind;
use dekopon_core::{AgentId, CapabilityId, Redacted, SecretDrn, bounded_problems, error_chain};
use dekopon_http_host::BoundCredential;
use thiserror::Error;

pub use config::{
    AgentBindingConfig, AssetsConfig, BrokerdConfig, CONFIG_API_VERSION, ConfigApiVersion,
    ConfigError, HostLimitsConfig, ManagedProviderSetConfig, PeerIdentity, PrincipalConfig,
    ResolvedConfig, ResolvedTelemetry, ServerLimitsConfig, StorageConfig, TelemetryConfig,
};
pub use credentials::{
    CREDENTIALS_API_VERSION, CredentialsError, HARD_MAX_CHATGPT_AUTH_BYTES, HARD_MAX_CREDENTIALS,
    HARD_MAX_CREDENTIALS_BYTES,
};
pub use dekopon_broker::MAX_SECRET_BINDINGS as HARD_MAX_SECRET_BINDINGS;
pub use provider_manager::{
    HARD_MAX_PROVIDER_COMPONENT_BYTES, HARD_MAX_PROVIDER_MANIFEST_BYTES,
    HARD_MAX_PROVIDER_STATE_BYTES, HARD_MAX_PROVIDER_STORE_BLOBS, HARD_MAX_PROVIDER_STORE_BYTES,
    PROVIDER_ARTIFACT_TYPE, PROVIDER_LAYER_MEDIA_TYPE, ProviderLock, ProviderLockApiVersion,
    ProviderManager, ProviderManagerError, ProviderManagerOptions, ProviderManagerPaths,
    ProviderSet, ProviderSetApiVersion, ProviderSkillStatus, ProviderStatus, ProviderSyncReport,
    ProviderVerifyReport,
};
pub use secrets::{
    HARD_MAX_SECRET_BYTES, HARD_MAX_SECRET_MAP_BYTES, HARD_MAX_SECRETS, SECRET_MAP_API_VERSION,
    SecretMapError, SourceError,
};
pub use server::{BrokerServer, MappedPeer, ServerError, ServerLimits, ShutdownDeadline};
pub use socket::{SocketError, SocketGuard, current_uid};

pub const HARD_MAX_PROVIDERS: usize = 64;

pub async fn telemetry_settings(
    config_path: impl AsRef<Path>,
    uid: u32,
) -> Result<Option<ResolvedTelemetry>, BrokerdError> {
    Ok(config::load(config_path, uid).await?.telemetry)
}

pub async fn run<F>(
    config_path: impl AsRef<Path>,
    shutdown: F,
    deadline: ShutdownDeadline,
) -> Result<(), BrokerdError>
where
    F: Future<Output = ()> + Send,
{
    serve(config_path, shutdown, deadline, lock_watch::POLL).await
}

async fn serve<F>(
    config_path: impl AsRef<Path>,
    shutdown: F,
    deadline: ShutdownDeadline,
    lock_poll: Duration,
) -> Result<(), BrokerdError>
where
    F: Future<Output = ()> + Send,
{
    let (candidate, report) = load_candidate(config_path.as_ref(), Mode::Boot).await;
    for warning in &report.warnings {
        warning.log();
    }
    let Some(candidate) = candidate else {
        return Err(BrokerdError::from_problems(report.problems));
    };
    let uid = current_uid();
    let limits = candidate.limits;
    let socket_path = candidate.socket_path.clone();
    let provider_lock = candidate.provider_lock.clone();
    let storage = candidate.storage.clone();
    let coordination = Arc::new(Coordination::new(
        config_path.as_ref().to_path_buf(),
        candidate,
    ));
    let server = BrokerServer::for_generations(Arc::clone(&coordination), limits)?
        .with_shutdown_deadline(deadline.clone());
    let (listener, mut socket_guard) = socket::bind(&socket_path, uid).await?;
    tracing::info!(event = "broker_started");
    let mut lock_stop = None;
    let stop = async {
        lock_stop = stopped(shutdown, provider_lock.as_ref(), uid, lock_poll).await;
        // Unlinking at admission close sends a new connect to NotFound, which clients retry,
        // instead of into a listener that will never accept it.
        if let Err(error) = socket_guard.cleanup() {
            tracing::warn!(
                event = "broker_socket_cleanup_failed",
                error = %error_chain(&error)
            );
        }
    };
    let serving = async {
        let result = server.serve(listener, stop).await;
        deadline.start(limits.shutdown_grace);
        result
    };
    let result = refresh_scheduler::serve(
        reaper::serve(
            serving,
            storage,
            || {
                coordination
                    .active()
                    .ready
                    .broker
                    .storage_retention_policies()
            },
            deadline.clone(),
        ),
        || coordination.active().ready.scheduled.clone(),
        deadline.clone(),
    )
    .await;

    // Checking result before cleanup here is deliberate: propagating cleanup's error first would
    // mask the real failure and skip logging broker_stopped.
    let cleanup = socket_guard.cleanup();
    if let Err(error) = &cleanup {
        tracing::warn!(
            event = "broker_socket_cleanup_failed",
            error = %error_chain(error)
        );
    }

    result?;
    tracing::info!(event = "broker_stopped");
    cleanup?;
    match lock_stop {
        None | Some(lock_watch::LockStop::Changed) => Ok(()),
        Some(lock_watch::LockStop::Unsettled) => Err(BrokerdError::ProviderLockUnsettled),
    }
}

async fn stopped<F>(
    shutdown: F,
    lock: Option<&lock_watch::LoadedLock>,
    uid: u32,
    poll: Duration,
) -> Option<lock_watch::LockStop>
where
    F: Future<Output = ()>,
{
    let Some(lock) = lock else {
        shutdown.await;
        return None;
    };
    tokio::select! {
        () = shutdown => None,
        stop = lock_watch::watch(lock, uid, poll) => Some(stop),
    }
}

/// Where `check` resolves a configuration's `providerSet`: the operator's provider set, synced into
/// a private directory that keeps the lock and blob store between checks.
#[derive(Debug)]
pub struct CheckProviders {
    pub provider_set: PathBuf,
    pub store: PathBuf,
}

#[derive(Debug, Default)]
pub struct CheckReport {
    pub problems: Vec<BrokerdError>,
    pub warnings: Vec<StartupWarning>,
    private_sources: Vec<PathBuf>,
}

/// Runs the startup validation `run` runs, then stops: no socket, no credentials file, no secret
/// map, no asset root and no provider storage root is touched.
pub async fn check(
    config_path: impl AsRef<Path>,
    providers: Option<CheckProviders>,
) -> CheckReport {
    load_candidate(config_path.as_ref(), Mode::Check(providers))
        .await
        .1
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    ProviderSet,
    FrameLimits,
    Runtime,
    Providers,
}

impl std::fmt::Display for Stage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ProviderSet => "provider set",
            Self::FrameLimits => "frame limits",
            Self::Runtime => "runtime files",
            Self::Providers => "providers, policy and capabilities",
        })
    }
}

enum Mode<'a> {
    Boot,
    Reload(&'a Candidate),
    Check(Option<CheckProviders>),
}

/// Only boot opens the storage host and asset directory: opening takes the root's writer lock and
/// empties the asset root, both of which the serving candidate owns while a reload is prepared.
enum Residents<'a> {
    Open,
    Borrow(&'a Candidate),
    Scratch,
}

struct Candidate {
    digest: String,
    fingerprint: Option<PrivateFingerprint>,
    ready: Ready,
    socket_path: PathBuf,
    provider_lock: Option<lock_watch::LoadedLock>,
    limits: ServerLimits,
    storage: Option<dekopon_storage_host::StorageHost>,
    assets: Option<dekopon_http_host::asset::AssetDirectory>,
    locked_providers: Option<Vec<dekopon_broker_host::LockedProviderSource>>,
    snapshot: config::ConfigSnapshot,
}

struct Coordination {
    config_path: PathBuf,
    loading: tokio::sync::Semaphore,
    state: parking_lot::Mutex<GenerationState>,
}

struct GenerationState {
    active: Arc<Candidate>,
    prepared: Option<(u32, Arc<Candidate>, Arc<Candidate>)>,
    reported: Option<RawCandidate>,
}

impl GenerationState {
    fn unreported(&mut self, raw: RawCandidate) -> bool {
        self.reported.replace(raw) != Some(raw)
    }

    fn owned_by(&self, uid: u32) -> bool {
        self.active
            .ready
            .identities
            .get(&uid)
            .is_some_and(MappedPeer::is_reconciliation_owner)
    }
}

impl Coordination {
    fn new(config_path: PathBuf, active: Candidate) -> Self {
        Self {
            config_path,
            loading: tokio::sync::Semaphore::new(1),
            state: parking_lot::Mutex::new(GenerationState {
                active: Arc::new(active),
                prepared: None,
                reported: None,
            }),
        }
    }

    fn active(&self) -> Arc<Candidate> {
        Arc::clone(&self.state.lock().active)
    }

    fn owns(&self, uid: u32) -> bool {
        self.state.lock().owned_by(uid)
    }

    async fn prepare(
        &self,
        expected_active: &str,
        uid: u32,
    ) -> (String, dekopon_broker_protocol::GenerationOutcome) {
        use dekopon_broker_protocol::GenerationOutcome;
        let Ok(_permit) = self.loading.acquire().await else {
            return (self.active().digest.clone(), GenerationOutcome::Refused);
        };
        let active = {
            let state = self.state.lock();
            if state.active.digest != expected_active
                || !state.owned_by(uid)
                || state
                    .prepared
                    .as_ref()
                    .is_some_and(|(owner, _, _)| *owner != uid)
            {
                return (state.active.digest.clone(), GenerationOutcome::Refused);
            }
            Arc::clone(&state.active)
        };
        let (next, report) = load_candidate(&self.config_path, Mode::Reload(&active)).await;
        let Some(next) = next else {
            let entries = report
                .problems
                .iter()
                .flat_map(reload_entries)
                .collect::<Vec<_>>();
            let count = entries.len();
            let error = bounded_problems(entries.into_iter());
            let raw = RawCandidate::of(
                self.config_path.clone(),
                report.private_sources,
                error.clone(),
            )
            .await;
            let unreported = self.state.lock().unreported(raw);
            let unreadable = report.problems.iter().any(|problem| {
                matches!(
                    problem,
                    BrokerdError::Config(config::ConfigError::Read { .. })
                )
            });
            match (unreported, unreadable) {
                (false, _) => {}
                (true, true) => {
                    tracing::warn!(event = "broker_reload_monitor_failed", applied = false, error = %error);
                }
                (true, false) => tracing::warn!(
                    event = "broker_reload_refused",
                    applied = false,
                    problems = count,
                    error = %error
                ),
            }
            return (active.digest.clone(), GenerationOutcome::Refused);
        };
        let next = {
            let mut state = self.state.lock();
            if !Arc::ptr_eq(&state.active, &active)
                || !state.owned_by(uid)
                || state
                    .prepared
                    .as_ref()
                    .is_some_and(|(owner, _, _)| *owner != uid)
            {
                return (state.active.digest.clone(), GenerationOutcome::Refused);
            }
            state.reported = None;
            if next.digest == state.active.digest
                && next.fingerprint.is_some()
                && next.fingerprint == state.active.fingerprint
            {
                state.prepared = None;
                return (next.digest.clone(), GenerationOutcome::Active);
            }
            let next = Arc::new(next);
            state.prepared = Some((uid, active, Arc::clone(&next)));
            next
        };
        next.ready.scheduled.inspect_records().await;
        (next.digest.clone(), GenerationOutcome::Prepared)
    }

    fn commit(&self, digest: &str, uid: u32) -> dekopon_broker_protocol::GenerationOutcome {
        use dekopon_broker_protocol::GenerationOutcome;
        let mut state = self.state.lock();
        if !state.owned_by(uid) {
            return GenerationOutcome::Refused;
        }
        let Some((owner, base, next)) = state.prepared.take() else {
            return GenerationOutcome::Refused;
        };
        if owner != uid || next.digest != digest || !Arc::ptr_eq(&base, &state.active) {
            state.prepared = Some((owner, base, next));
            return GenerationOutcome::Refused;
        }
        let categories = state.active.snapshot.changed_categories(&next.snapshot);
        state.active = next;
        tracing::info!(
            event = "broker_reload_applied",
            generation.digest = digest,
            category.count = categories.len(),
            categories = %categories.join(",")
        );
        GenerationOutcome::Applied
    }

    fn abort(&self, digest: &str, uid: u32) -> dekopon_broker_protocol::GenerationOutcome {
        use dekopon_broker_protocol::GenerationOutcome;
        let mut state = self.state.lock();
        if state.owned_by(uid)
            && state
                .prepared
                .as_ref()
                .is_some_and(|(owner, _, next)| *owner == uid && next.digest == digest)
        {
            state.prepared = None;
            GenerationOutcome::Aborted
        } else {
            GenerationOutcome::Refused
        }
    }
}

/// Every stage whose inputs loaded runs, so one report names every problem; a stage whose inputs
/// did not is reported as skipped.
#[expect(
    clippy::too_many_lines,
    reason = "independent validation stages aggregate failures before preparing a candidate"
)]
async fn load_candidate(config_path: &Path, mode: Mode<'_>) -> (Option<Candidate>, CheckReport) {
    let mut report = CheckReport::default();
    let uid = current_uid();
    let load_mode = match mode {
        Mode::Boot => LoadMode::Boot,
        Mode::Reload(active) => LoadMode::Reload {
            locked_providers: active.locked_providers.as_deref(),
            provider_lock: active.provider_lock.as_ref(),
        },
        Mode::Check(_) => LoadMode::Check,
    };
    let Some(mut config) = (match config::load_in(config_path, uid, load_mode).await {
        Ok((config, refusal)) => {
            report.problems.extend(refusal.map(BrokerdError::from));
            report.private_sources.extend(
                [
                    &config.credentials_path,
                    &config.secret_map_path,
                    &config.policies_path,
                ]
                .into_iter()
                .flatten()
                .cloned(),
            );
            Some(config)
        }
        Err(error) => {
            report.problems.push(error.into());
            None
        }
    }) else {
        for stage in [
            Stage::ProviderSet,
            Stage::FrameLimits,
            Stage::Runtime,
            Stage::Providers,
        ] {
            report.skipped(stage);
        }
        return (None, report);
    };
    let (providers_resolved, residents) = match mode {
        Mode::Boot => (true, Residents::Open),
        Mode::Reload(active) => (true, Residents::Borrow(active)),
        Mode::Check(providers) => {
            if config.identities.is_empty() {
                report.warnings.push(StartupWarning::IdentitiesRequired);
            }
            // A Check load leaves a providerSet's providers for this function to resolve.
            let resolved = match (config.providers.is_empty(), providers) {
                (true, Some(providers)) => match resolve_provider_set(providers, uid).await {
                    Ok(sources) => {
                        config.providers = sources
                            .iter()
                            .map(|source| source.path().to_path_buf())
                            .collect();
                        config.locked_providers = Some(sources);
                        true
                    }
                    Err(error) => {
                        report.problems.push(error);
                        false
                    }
                },
                (true, None) => {
                    report.problems.push(BrokerdError::ProviderSetRequired);
                    false
                }
                (false, Some(_)) => {
                    report.problems.push(BrokerdError::ProviderSetUnused);
                    true
                }
                (false, None) => true,
            };
            (resolved, Residents::Scratch)
        }
    };
    let restart_categories = match residents {
        Residents::Borrow(active) => active.snapshot.restart_required(&config.snapshot),
        Residents::Open | Residents::Scratch => Vec::new(),
    };
    if !restart_categories.is_empty() {
        report.problems.push(BrokerdError::RestartRequired {
            categories: restart_categories.clone(),
        });
    }
    let frame_limits = keep(
        &mut report.problems,
        config
            .server_limits
            .frame_limits()
            .map_err(BrokerdError::from),
    );
    let mut scratch = None;
    let runtime = match residents {
        Residents::Open => boot_runtime(&mut config, uid, &mut report.problems).await,
        Residents::Borrow(active) => {
            reload_runtime(&mut config, uid, active, &mut scratch, &mut report.problems).await
        }
        Residents::Scratch => {
            keep(&mut report.problems, scratch_host(&config, &mut scratch)).map(|storage| Runtime {
                credentials: match config.credentials_path {
                    Some(_) => Credentials::Unchecked,
                    None => Credentials::Loaded(CredentialStore::empty()),
                },
                secrets: match config.secret_map_path {
                    Some(_) => Secrets::Unchecked,
                    None => Secrets::Loaded(
                        dekopon_broker::SecretCatalog::empty(),
                        secrets::ScheduledCatalog::default(),
                    ),
                },
                fingerprint: None,
                storage,
                assets: None,
            })
        }
    };
    let (Some(frame_limits), Some(mut runtime), true) = (frame_limits, runtime, providers_resolved)
    else {
        report.skipped(Stage::Providers);
        return (None, report);
    };
    let limits = ServerLimits {
        frame: frame_limits,
        max_connections: config.server_limits.max_connections,
        shutdown_grace: config.server_limits.shutdown_grace(),
    };
    let socket_path = std::mem::take(&mut config.socket_path);
    let provider_lock = config.provider_lock.take();
    let locked_providers = config.locked_providers.clone();
    let reuse_registry = provider_lock.is_some() || restart_categories.is_empty();
    let storage = runtime.storage.clone();
    let assets = runtime.assets.clone();
    let fingerprint = runtime.fingerprint.take();
    let snapshot = config.snapshot.clone();
    let resident_registry = match residents {
        Residents::Borrow(active) if reuse_registry => Some(active.ready.registry.clone()),
        Residents::Open | Residents::Borrow(_) | Residents::Scratch => None,
    };
    let prepared = prepare(
        config,
        frame_limits.max_frame_bytes,
        runtime,
        &mut report.warnings,
        resident_registry,
    )
    .await;
    drop(scratch);
    match prepared {
        Ok(ready) if report.problems.is_empty() => {
            let digest = match snapshot.digest() {
                Ok(digest) => digest,
                Err(source) => {
                    report.problems.push(BrokerdError::Snapshot { source });
                    return (None, report);
                }
            };
            (
                Some(Candidate {
                    digest,
                    fingerprint,
                    ready,
                    socket_path,
                    provider_lock,
                    limits,
                    storage,
                    assets,
                    locked_providers,
                    snapshot,
                }),
                report,
            )
        }
        Ok(_) => (None, report),
        Err(problems) => {
            report.problems.extend(problems);
            (None, report)
        }
    }
}

impl CheckReport {
    fn skipped(&mut self, stage: Stage) {
        self.warnings.push(StartupWarning::Skipped { stage });
    }
}

async fn boot_runtime(
    config: &mut ResolvedConfig,
    uid: u32,
    problems: &mut Vec<BrokerdError>,
) -> Option<Runtime> {
    let private = private_inputs(config, uid, problems).await;
    if let Some(private) = &private {
        private.scheduled.inspect_records().await;
    }
    let storage = keep(
        problems,
        config
            .storage
            .as_ref()
            .map(|storage| {
                dekopon_storage_host::StorageHost::open(&storage.root_path, storage.limits.clone())
            })
            .transpose()
            .map_err(BrokerdError::Storage),
    );
    tracing::info!(
        max_connections = config.server_limits.max_connections,
        max_memory_bytes = config.host_limits.max_memory_bytes,
        aggregate_ceiling_bytes = config.host_options.max_total_memory_bytes,
        cwasm_cache = config
            .host_options
            .cwasm_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        "broker provider guest-memory budget"
    );
    if !config.plaintext_hosts.is_empty() {
        let hosts = config.plaintext_hosts.iter().collect::<Vec<_>>().join(", ");
        tracing::info!(
            event = "broker_plaintext_hosts",
            "http plaintext hosts allowed: [{hosts}]"
        );
    }
    let assets = match &config.assets {
        Some(config) => keep(
            problems,
            assets::initialize(config)
                .await
                .map(Some)
                .map_err(BrokerdError::Assets),
        ),
        None => Some(None),
    };
    let (private, storage, assets) = (private?, storage?, assets?);
    Some(Runtime {
        credentials: Credentials::Loaded(private.credentials),
        secrets: Secrets::Loaded(private.secrets, private.scheduled),
        fingerprint: Some(private.fingerprint),
        storage,
        assets,
    })
}

/// A changed `storage` or `assets` already refuses as restart-required, so the rest of the
/// candidate is validated against a scratch host and no asset directory instead of opening either.
async fn reload_runtime(
    config: &mut ResolvedConfig,
    uid: u32,
    active: &Candidate,
    scratch: &mut Option<tempfile::TempDir>,
    problems: &mut Vec<BrokerdError>,
) -> Option<Runtime> {
    let private = private_inputs(config, uid, problems).await;
    let storage = if active.snapshot.keeps(&config.snapshot, Resident::Storage) {
        active.storage.clone()
    } else {
        keep(problems, scratch_host(config, scratch))?
    };
    let assets = if active.snapshot.keeps(&config.snapshot, Resident::Assets) {
        active.assets.clone()
    } else {
        None
    };
    let private = private?;
    Some(Runtime {
        credentials: Credentials::Loaded(private.credentials),
        secrets: Secrets::Loaded(private.secrets, private.scheduled),
        fingerprint: Some(private.fingerprint),
        storage,
        assets,
    })
}

#[cfg(test)]
tokio::task_local! {
    pub(crate) static AFTER_PRIVATE_READ: std::sync::Arc<dyn Fn() + Send + Sync>;
}

async fn private_inputs(
    config: &mut ResolvedConfig,
    uid: u32,
    problems: &mut Vec<BrokerdError>,
) -> Option<PrivateInputs> {
    match socket::validate_socket_parent(&config.socket_path, uid) {
        // An owner-only socket under a private parent means other configured UIDs could never
        // connect — the broker starts healthy while peers loop on EACCES — so every unreachable
        // peer is checked at startup.
        Ok(parent) if dekopon_broker_protocol::ipc_socket_mode(&parent) == 0o600 => {
            let configured = config
                .identities
                .iter()
                .map(|identity| identity.uid)
                .filter(|peer| *peer != uid)
                .collect::<Vec<_>>();
            if !configured.is_empty() {
                problems.push(BrokerdError::UnreachablePeerUids {
                    configured,
                    server: uid,
                });
            }
        }
        Ok(_) => {}
        Err(error) => problems.push(error.into()),
    }
    // A compilation cache holds compiled code the broker will execute. Anyone who can write into
    // it can choose what the privileged process runs, so it must sit under a private parent.
    if let Some(cache) = &config.host_options.cwasm_dir
        && let Err(error) = socket::validate_private_parent(cache, uid)
    {
        problems.push(error.into());
    }
    let credentials = match &config.credentials_path {
        Some(path) => keep(
            problems,
            credentials::load_with_bytes(path, uid)
                .await
                .map(Some)
                .map_err(BrokerdError::from),
        ),
        None => Some(None),
    };
    let secrets = match &config.secret_map_path {
        Some(path) => keep(
            problems,
            secrets::load_with_bytes(path, uid)
                .await
                .map(Some)
                .map_err(BrokerdError::from),
        ),
        None => Some(None),
    };
    #[cfg(test)]
    let _hook_ran = AFTER_PRIVATE_READ.try_with(|hook| hook()).is_ok();
    let (Some(credentials), Some(secrets)) = (credentials, secrets) else {
        return None;
    };
    if let Err(error) = config.snapshot.record_authority_references(
        credentials.as_ref().map(|loaded| &loaded.references),
        secrets.as_ref().map(|(_, bytes, _)| bytes.as_slice()),
    ) {
        problems.push(error.into());
    }
    let fingerprint = PrivateFingerprint::of(
        credentials.as_ref().map(|loaded| &loaded.private_digest),
        secrets.as_ref().map(|(_, bytes, _)| bytes.as_slice()),
    );
    let (catalog, scheduled) = secrets.map_or_else(
        || {
            (
                dekopon_broker::SecretCatalog::empty(),
                secrets::ScheduledCatalog::default(),
            )
        },
        |(catalog, _, scheduled)| (catalog, scheduled),
    );
    Some(PrivateInputs {
        credentials: credentials.map_or_else(CredentialStore::empty, |loaded| loaded.store),
        secrets: catalog,
        scheduled,
        fingerprint,
    })
}

struct PrivateInputs {
    credentials: CredentialStore,
    secrets: dekopon_broker::SecretCatalog,
    scheduled: secrets::ScheduledCatalog,
    fingerprint: PrivateFingerprint,
}

/// Held only in memory and never logged or sent: it tells a same-digest credential rotation from
/// an unchanged reload poll without any private byte leaving the broker.
#[derive(PartialEq, Eq)]
struct PrivateFingerprint([u8; 32]);

impl PrivateFingerprint {
    fn of(credentials: Option<&[u8; 32]>, secret_map: Option<&[u8]>) -> Self {
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        match credentials {
            Some(digest) => {
                hasher.update([1]);
                hasher.update(digest);
            }
            None => hasher.update([0]),
        }
        match secret_map {
            Some(bytes) => {
                hasher.update([1]);
                hasher.update(bytes);
            }
            None => hasher.update([0]),
        }
        Self(hasher.finalize().into())
    }
}

const MAX_RAW_SOURCE_BYTES: u64 = 16 << 20;

/// In memory only, never logged or sent: it covers private bytes and exists to tell one refused
/// candidate from another.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RawCandidate([u8; 32]);

impl RawCandidate {
    async fn of(config: PathBuf, private: Vec<PathBuf>, refusal: String) -> Self {
        use std::io::Read as _;

        use sha2::{Digest as _, Sha256};
        let digest = tokio::task::spawn_blocking(move || {
            let mut paths = std::fs::read_dir(&config).map_or_else(
                |_| vec![config.clone()],
                |entries| {
                    entries
                        .filter_map(|entry| Some(entry.ok()?.path()))
                        .collect()
                },
            );
            paths.sort();
            paths.extend(private);
            let mut hasher = Sha256::new();
            for path in paths {
                let bytes = std::fs::metadata(&path)
                    .and_then(|metadata| {
                        if !metadata.is_file() {
                            return Err(std::io::ErrorKind::InvalidInput.into());
                        }
                        let mut bytes = Vec::new();
                        std::fs::File::open(&path)?
                            .take(MAX_RAW_SOURCE_BYTES)
                            .read_to_end(&mut bytes)?;
                        Ok(bytes)
                    })
                    .unwrap_or_else(|error| error.to_string().into_bytes());
                for part in [path.as_os_str().as_encoded_bytes(), &bytes] {
                    hasher.update(part.len().to_le_bytes());
                    hasher.update(part);
                }
            }
            hasher.update(refusal.as_bytes());
            hasher.finalize().into()
        })
        .await;
        Self(digest.unwrap_or_default())
    }
}

async fn resolve_provider_set(
    providers: CheckProviders,
    uid: u32,
) -> Result<Vec<dekopon_broker_host::LockedProviderSource>, BrokerdError> {
    let mut directory = tokio::fs::DirBuilder::new();
    directory.mode(0o700);
    match directory.create(&providers.store).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(BrokerdError::CheckDirectory {
                path: providers.store,
                source,
            });
        }
    }
    let lock_file = providers.store.join("providers.lock.yaml");
    let store = providers.store.join("store");
    let manager = ProviderManager::new(ProviderManagerOptions {
        paths: ProviderManagerPaths {
            provider_set: Some(providers.provider_set),
            lock_file: lock_file.clone(),
            store: store.clone(),
        },
        plaintext_loopback_registries: Vec::new(),
    })
    .map_err(BrokerdError::ProviderSet)?;
    manager.sync().await.map_err(BrokerdError::ProviderSet)?;
    manager
        .precompile()
        .await
        .map_err(BrokerdError::ProviderSet)?;
    provider_manager::load_locked_sources(&lock_file, &store, uid)
        .await
        .map_err(BrokerdError::ProviderSet)
}

fn scratch_host(
    config: &ResolvedConfig,
    scratch: &mut Option<tempfile::TempDir>,
) -> Result<Option<dekopon_storage_host::StorageHost>, BrokerdError> {
    let Some(storage) = &config.storage else {
        return Ok(None);
    };
    let (directory, host) = scratch_storage(storage)?;
    *scratch = Some(directory);
    Ok(Some(host))
}

/// Provider storage is opened in a throwaway directory so storage-backed constraints and chat
/// memory are validated against a real host without touching the configured root.
fn scratch_storage(
    storage: &StorageConfig,
) -> Result<(tempfile::TempDir, dekopon_storage_host::StorageHost), BrokerdError> {
    let scratch = tempfile::tempdir().map_err(|source| BrokerdError::CheckDirectory {
        path: std::env::temp_dir(),
        source,
    })?;
    let root = std::fs::canonicalize(scratch.path())
        .map_err(|source| BrokerdError::CheckDirectory {
            path: scratch.path().to_path_buf(),
            source,
        })?
        .join("storage");
    let host = dekopon_storage_host::StorageHost::open(root, storage.limits.clone())
        .map_err(BrokerdError::Storage)?;
    Ok((scratch, host))
}

enum Credentials {
    Loaded(CredentialStore),
    Unchecked,
}

enum Secrets {
    Loaded(dekopon_broker::SecretCatalog, secrets::ScheduledCatalog),
    Unchecked,
}

struct Runtime {
    credentials: Credentials,
    secrets: Secrets,
    fingerprint: Option<PrivateFingerprint>,
    storage: Option<dekopon_storage_host::StorageHost>,
    assets: Option<dekopon_http_host::asset::AssetDirectory>,
}

struct Ready {
    broker: Arc<Broker<TraceOnlyAuditLog>>,
    scheduled: secrets::ScheduledCatalog,
    identities: BTreeMap<u32, MappedPeer>,
    registry: BrokerProviderRegistry,
}

/// Every problem found before the broker is built is returned together; the policy and the
/// constraint sets are independent, so one refusing does not hide the other.
#[expect(
    clippy::too_many_lines,
    reason = "reshaped by the unit that next rewrites this"
)]
async fn prepare(
    config: ResolvedConfig,
    max_frame_bytes: usize,
    runtime: Runtime,
    warnings: &mut Vec<StartupWarning>,
    resident_registry: Option<BrokerProviderRegistry>,
) -> Result<Ready, Vec<BrokerdError>> {
    let uid = current_uid();
    // Locked artifacts were checked at boot; a pruned old blob is not a new authority source.
    for provider in config
        .providers
        .iter()
        .skip(if resident_registry.is_some() {
            config.locked_providers.as_ref().map_or(0, Vec::len)
        } else {
            0
        })
    {
        socket::validate_owned_file(provider, uid).map_err(|error| vec![error.into()])?;
    }
    let mut registry = if let Some(registry) = resident_registry {
        Ok(registry)
    } else {
        match config.locked_providers {
            Some(sources) => {
                BrokerProviderRegistry::load_locked_with_options(
                    sources,
                    config.host_limits,
                    runtime.storage,
                    &config.host_options,
                )
                .await
            }
            None => {
                BrokerProviderRegistry::load_with_options(
                    config.providers,
                    config.host_limits,
                    runtime.storage,
                    &config.host_options,
                )
                .await
            }
        }
    }
    .map_err(|error| vec![BrokerdError::Host(error)])?;
    if let Some(directory) = runtime.assets {
        registry.set_assets(directory);
    }
    let mut problems = Vec::new();
    if let Err(error) = validate_manifest_metadata(
        &registry,
        max_frame_bytes.saturating_sub(config::MINIMUM_RESPONSE_OVERHEAD_BYTES),
    ) {
        problems.push(error);
    }
    let identity_directory = keep(
        &mut problems,
        IdentityDirectory::new(config.principals.iter().flat_map(|(principal, entry)| {
            entry
                .subjects
                .iter()
                .map(|subject| (subject.clone(), principal.clone()))
        }))
        .map_err(BrokerdError::Broker),
    );
    let world = keep(
        &mut problems,
        PolicyWorld::new(
            config
                .identities
                .iter()
                .map(|identity| identity.principal.clone())
                .chain(config.principals.keys().cloned()),
            registry
                .capabilities()
                .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
        )
        .map(|world| {
            world
                .with_group_members(config.principals.iter().flat_map(|(principal, entry)| {
                    entry
                        .groups
                        .iter()
                        .map(|group| (principal.clone(), group.clone()))
                }))
                .with_read_only(
                    registry
                        .capabilities()
                        .filter(|(_, capability)| capability.effect == EffectKind::ReadOnly)
                        .map(|(_, capability)| capability.id.clone()),
                )
        })
        .map_err(|source| BrokerdError::Policy { source }),
    );
    let (secret_catalog, scheduled, secrets_unchecked) = match runtime.secrets {
        Secrets::Loaded(catalog, scheduled) => (Some(catalog), Some(scheduled), false),
        Secrets::Unchecked => (None, None, true),
    };
    let policy = world.and_then(|world| {
        let world = world.with_secrets(
            secret_catalog
                .iter()
                .flat_map(|catalog| catalog.drns().cloned()),
        );
        keep(
            &mut problems,
            build_policy(
                &config.policies,
                world,
                config.strict,
                secrets_unchecked,
                warnings,
            ),
        )
    });
    let (constraint_sets, capability_problems) =
        capabilities::constraint_sets(&config.capabilities, |provider| {
            let declared = registry
                .capabilities()
                .filter(|(owner, _)| *owner == provider)
                .map(|(_, capability)| capabilities::ManifestCapability {
                    id: &capability.id,
                    effect: capability.effect,
                    risk: capability.risk,
                })
                .collect::<Vec<_>>();
            (!declared.is_empty()).then_some(declared)
        });
    let (tolerable, fatal): (Vec<_>, Vec<_>) = capability_problems
        .into_iter()
        .partition(|problem| problem.is_unloaded_name() && !config.strict);
    if !fatal.is_empty() {
        problems.push(BrokerdError::Capabilities { problems: fatal });
    }
    warnings.extend(
        tolerable
            .into_iter()
            .map(StartupWarning::UnloadedCapability),
    );
    let (Some(identity_directory), Some(policy), true) =
        (identity_directory, policy, problems.is_empty())
    else {
        return Err(problems);
    };
    let credential_store = match runtime.credentials {
        Credentials::Loaded(store) => store,
        Credentials::Unchecked => unchecked_credentials(&constraint_sets, &config.agents, warnings)
            .map_err(|error| vec![error])?,
    };
    let constraints = ConstraintCatalog::new(constraint_sets)
        .map_err(|error| vec![BrokerdError::Broker(error)])?
        .with_agent_credentials(
            config
                .agents
                .into_iter()
                .map(|(agent, binding)| (agent, binding.credentials))
                .collect(),
        );
    let leniency = if config.strict {
        Leniency::Strict
    } else {
        Leniency::Tolerant
    };
    let shared_registry = registry.clone();
    let (broker, broker_warnings) = Broker::start(
        registry,
        BROKER_PRINCIPAL
            .parse()
            .expect("the broker principal is a valid identifier"),
        policy,
        constraints,
        credential_store,
        identity_directory,
        Arc::new(TraceOnlyAuditLog),
        config.broker_limits,
        leniency,
    )
    .map_err(|error| vec![BrokerdError::Broker(error)])?;
    warnings.extend(broker_warnings.into_iter().map(StartupWarning::Broker));
    let broker = match secret_catalog {
        Some(catalog) => broker
            .with_secret_catalog(catalog)
            .map_err(|error| vec![BrokerdError::Broker(error)])?,
        None => broker,
    };
    let broker = match config.chat_memory {
        Some(memory) => broker
            .with_chat_memory(memory)
            .map_err(|error| vec![BrokerdError::Broker(error)])?,
        None => broker,
    };
    let mut identities = BTreeMap::new();
    for identity in config.identities {
        identities.insert(
            identity.uid,
            MappedPeer {
                context: identity
                    .context()
                    .map_err(|error| vec![BrokerdError::Context(error)])?,
                attestor: identity.attestor,
            },
        );
    }
    validate_capability_responses(&broker, &identities, max_frame_bytes)
        .map_err(|error| vec![error])?;
    Ok(Ready {
        broker: Arc::new(broker),
        scheduled: scheduled.unwrap_or_default(),
        identities,
        registry: shared_registry,
    })
}

fn keep<T>(problems: &mut Vec<BrokerdError>, result: Result<T, BrokerdError>) -> Option<T> {
    result.map_err(|error| problems.push(error)).ok()
}

/// A secret map is broker-private and absent from a checkout, so an unchecked build admits each
/// secret a policy names and reports it, and keeps validating everything else.
fn build_policy(
    policies: &str,
    mut world: PolicyWorld,
    strict: bool,
    secrets_unchecked: bool,
    warnings: &mut Vec<StartupWarning>,
) -> Result<PolicyEngine, BrokerdError> {
    let mut admitted = BTreeSet::new();
    loop {
        let built = if strict {
            PolicyEngine::new(policies, &world).map(|policy| (policy, Vec::new()))
        } else {
            PolicyEngine::new_lenient(policies, &world)
        };
        let source = match built {
            Ok((policy, unresolved)) => {
                warnings.extend(unresolved.into_iter().map(|entry| {
                    StartupWarning::UnresolvedPolicyName {
                        policy: entry.policy,
                        kind: entry.kind.label(),
                        name: entry.name,
                    }
                }));
                return Ok(policy);
            }
            Err(source) => source,
        };
        let PolicyBuildError::UnknownSecret { policy, secret } = &source else {
            return Err(BrokerdError::Policy { source });
        };
        let drn = match secret.parse::<SecretDrn>() {
            Ok(drn) if secrets_unchecked && admitted.insert(drn.clone()) => drn,
            Ok(_) | Err(_) => return Err(BrokerdError::Policy { source }),
        };
        warnings.push(StartupWarning::SecretRequired {
            policy: policy.clone(),
            secret: secret.clone(),
        });
        world = world.with_secrets([drn]);
    }
}

/// Never leaves this process: a check builds no server, so no request can carry it.
const UNCHECKED_CREDENTIAL: &str = "dekopon-check-placeholder";
const UNCHECKED_DESTINATION: &str = "credential.check.invalid";

/// Stands in for each credential the constraint sets can select, bound to exactly the hosts that
/// select it, so `Broker::start` still proves every other credential rule.
fn unchecked_credentials(
    sets: &BTreeMap<CapabilityId, ConstraintSet>,
    agents: &BTreeMap<AgentId, AgentBindingConfig>,
    warnings: &mut Vec<StartupWarning>,
) -> Result<CredentialStore, BrokerdError> {
    let mut destinations = BTreeMap::<&str, BTreeSet<&str>>::new();
    for set in sets.values() {
        let Some(name) = set.credential.as_deref() else {
            continue;
        };
        let hosts = set
            .constraints
            .http
            .iter()
            .flat_map(|http| http.allowed_hosts.iter().map(String::as_str));
        let rebound = agents
            .values()
            .filter_map(|binding| binding.credentials.get(name).map(String::as_str));
        for selectable in std::iter::once(name).chain(rebound) {
            destinations
                .entry(selectable)
                .or_default()
                .extend(hosts.clone());
        }
    }
    let mut entries = Vec::with_capacity(destinations.len());
    for (name, hosts) in destinations {
        warnings.push(StartupWarning::CredentialRequired {
            name: name.to_owned(),
        });
        let placeholder = |hosts: Vec<String>| {
            BoundCredential::bearer(
                "Bearer",
                Redacted::new(UNCHECKED_CREDENTIAL.to_owned()),
                hosts,
            )
        };
        let credential = placeholder(hosts.into_iter().map(str::to_owned).collect())
            .or_else(|_| placeholder(vec![UNCHECKED_DESTINATION.to_owned()]))
            .map_err(BrokerdError::UncheckedCredential)?;
        entries.push((name.to_owned(), credential));
    }
    CredentialStore::new(entries).map_err(BrokerdError::Broker)
}

#[derive(Debug, Error)]
pub enum StartupWarning {
    #[error(
        "policy {policy} names {kind} {name:?}, which no loaded provider declares; it can never match"
    )]
    UnresolvedPolicyName {
        policy: String,
        kind: &'static str,
        name: String,
    },
    #[error("{0}")]
    UnloadedCapability(capabilities::CapabilityProblem),
    #[error("{0}")]
    Broker(dekopon_broker::StartupWarning),
    #[error("credential {name} is selected by a capability; boot requires it in credentialsPath")]
    CredentialRequired { name: String },
    #[error("policy {policy} names secret {secret}; boot requires it in secretMapPath")]
    SecretRequired { policy: String, secret: String },
    #[error("no identities; boot requires them, from a fragment or the chart's peers.yaml")]
    IdentitiesRequired,
    #[error("{stage} not checked: a stage it needs failed")]
    Skipped { stage: Stage },
}

impl StartupWarning {
    fn log(&self) {
        match self {
            Self::UnresolvedPolicyName { policy, kind, name } => tracing::warn!(
                target: "dekopon_brokerd::audit",
                {
                    audit.event = "policy.name.unresolved",
                    policy.id = %policy,
                    name.kind = kind,
                    name = %name,
                },
                "{self}"
            ),
            Self::UnloadedCapability(problem) => tracing::warn!(
                target: "dekopon_brokerd::audit",
                { audit.event = "config.startup.warning", reason = "unloaded-capability" },
                "{problem}"
            ),
            Self::Broker(warning) => tracing::warn!(
                target: "dekopon_brokerd::audit",
                {
                    audit.event = "config.startup.warning",
                    reason = warning.reason(),
                    capability.id = %warning.capability(),
                },
                "{warning}"
            ),
            Self::Skipped { .. } => tracing::warn!(
                target: "dekopon_brokerd::audit",
                { audit.event = "config.startup.warning", reason = "skipped-stage" },
                "{self}"
            ),
            Self::CredentialRequired { .. }
            | Self::SecretRequired { .. }
            | Self::IdentitiesRequired => tracing::warn!(
                target: "dekopon_brokerd::audit",
                { audit.event = "config.startup.warning", reason = "unchecked-reference" },
                "{self}"
            ),
        }
    }
}

fn validate_capability_responses<A: AuditLog>(
    broker: &Broker<A>,
    identities: &BTreeMap<u32, MappedPeer>,
    maximum: usize,
) -> Result<(), BrokerdError> {
    for peer in identities.values() {
        let (capabilities, command_words, command_word_help) =
            broker.capability_view(&peer.context);
        let response =
            ResponseEnvelope::capabilities(capabilities, command_words, command_word_help);
        let length = encoded_capability_response(&response)?;
        if length > maximum {
            return Err(BrokerdError::CapabilityResponseTooLarge { length, maximum });
        }
    }
    // Direct peers are granted almost nothing in a gateway deployment; the real capability sets are
    // reachable only through attested sessions that can't be enumerated at startup, so the broker
    // bounds their worst case instead.
    let (capabilities, command_words, command_word_help) = broker.capability_ceiling();
    let response = ResponseEnvelope::chat_capabilities(
        capabilities,
        command_words,
        command_word_help,
        broker.chat_memory_ceiling(),
        broker.principal_ceiling(),
    );
    let length = encoded_capability_response(&response)?;
    if length > maximum {
        return Err(BrokerdError::CapabilityCeilingTooLarge { length, maximum });
    }
    Ok(())
}

fn encoded_capability_response(response: &ResponseEnvelope) -> Result<usize, BrokerdError> {
    Ok(serde_json::to_vec(response)
        .map_err(|source| BrokerdError::CapabilityResponse { source })?
        .len())
}

fn validate_manifest_metadata(
    registry: &BrokerProviderRegistry,
    maximum: usize,
) -> Result<(), BrokerdError> {
    let mut length = 0_usize;
    for manifest in registry.manifests() {
        let encoded = serde_json::to_vec(manifest)
            .map_err(|source| BrokerdError::ManifestMetadata { source })?;
        length =
            length
                .checked_add(encoded.len())
                .ok_or(BrokerdError::ManifestMetadataTooLarge {
                    length: usize::MAX,
                    maximum,
                })?;
        if length > maximum {
            return Err(BrokerdError::ManifestMetadataTooLarge { length, maximum });
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum BrokerdError {
    #[error("broker socket security validation failed")]
    Socket(#[from] SocketError),
    #[error("broker configuration is invalid")]
    Config(#[from] ConfigError),
    #[error("capability configuration is invalid: {}", render_problems(.problems))]
    Capabilities {
        problems: Vec<capabilities::CapabilityProblem>,
    },
    #[error("broker credentials are unavailable or invalid")]
    Credentials(#[from] CredentialsError),
    #[error("broker private secret map is unavailable or invalid")]
    Secrets(#[from] SecretMapError),
    #[error("broker provider storage could not start")]
    Storage(#[source] dekopon_storage_host::StorageHostError),
    #[error("broker storage reaper task failed")]
    StorageReaper(#[source] tokio::task::JoinError),
    #[error("broker assets could not start")]
    Assets(#[source] assets::AssetsStartupError),
    #[error("broker provider host could not start")]
    Host(#[source] dekopon_broker_host::BrokerHostError),
    #[error("broker generation snapshot could not be encoded")]
    Snapshot {
        #[source]
        source: serde_json::Error,
    },
    #[error("broker provider metadata could not be encoded")]
    ManifestMetadata {
        #[source]
        source: serde_json::Error,
    },
    #[error("broker provider metadata is {length} bytes; maximum is {maximum}")]
    ManifestMetadataTooLarge { length: usize, maximum: usize },
    #[error("broker capability response could not be encoded")]
    CapabilityResponse {
        #[source]
        source: serde_json::Error,
    },
    #[error("broker capability response is {length} bytes; frame maximum is {maximum}")]
    CapabilityResponseTooLarge { length: usize, maximum: usize },
    #[error(
        "broker could answer a session with a {length}-byte capability response; frame maximum is {maximum}"
    )]
    CapabilityCeilingTooLarge { length: usize, maximum: usize },
    #[error("broker policy could not start")]
    Broker(#[source] dekopon_broker::BrokerBuildError),
    #[error("broker policy set is invalid")]
    Policy {
        #[source]
        source: PolicyBuildError,
    },
    #[error("broker peer identity is invalid")]
    Context(#[source] dekopon_broker::ContextError),
    #[error(
        "broker socket parent grants no group traversal, so the socket is owner-only for server UID {server}; configured peer UID(s) {} can never connect",
        .configured.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
    )]
    UnreachablePeerUids { configured: Vec<u32>, server: u32 },
    #[error("broker server failed")]
    Server(#[from] ServerError),
    #[error("{}", render_errors(.problems))]
    Startup { problems: Vec<BrokerdError> },
    #[error("broker configuration changed restart-required categories: {categories:?}")]
    RestartRequired { categories: Vec<String> },
    #[error("the configuration names a providerSet; check it with --provider-set and --store")]
    ProviderSetRequired,
    #[error("the configuration names provider paths; --provider-set does not apply to it")]
    ProviderSetUnused,
    #[error("the provider set could not be resolved")]
    ProviderSet(#[source] ProviderManagerError),
    #[error(
        "the provider lock never read as one stable file; the broker drained so a restart rereads it"
    )]
    ProviderLockUnsettled,
    #[error("check could not prepare its private directory {path}")]
    CheckDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("check could not stand in for a configured credential")]
    UncheckedCredential(#[source] dekopon_http_host::ConfigurationError),
}

impl BrokerdError {
    fn from_problems(mut problems: Vec<Self>) -> Self {
        match (problems.pop(), problems.is_empty()) {
            (Some(only), true) => only,
            (Some(last), false) => {
                problems.push(last);
                Self::Startup { problems }
            }
            (None, _) => Self::Startup { problems },
        }
    }
}

fn reload_entries(problem: &BrokerdError) -> Vec<String> {
    if let BrokerdError::Credentials(CredentialsError::Invalid { problems }) = problem {
        problems
            .iter()
            .map(|item| format!("broker credentials are invalid: {item}"))
            .collect()
    } else if let BrokerdError::Secrets(SecretMapError::Validation { problems }) = problem {
        problems
            .iter()
            .map(|item| format!("private secret map has conflicts: {item}"))
            .collect()
    } else if let BrokerdError::Capabilities { problems } = problem {
        problems
            .iter()
            .map(|item| format!("capability configuration is invalid: {item}"))
            .collect()
    } else if let BrokerdError::Config(error) = problem {
        vec![format!("{problem}: {error}")]
    } else if let BrokerdError::Credentials(error) = problem {
        vec![format!("{problem}: {error}")]
    } else if let BrokerdError::Secrets(error) = problem {
        vec![format!("{problem}: {error}")]
    } else {
        vec![problem.to_string()]
    }
}

fn render_errors(problems: &[BrokerdError]) -> String {
    problems
        .iter()
        .map(|problem| error_chain(problem))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests;

fn render_problems(problems: &[capabilities::CapabilityProblem]) -> String {
    problems
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}
