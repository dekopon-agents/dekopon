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
mod config;
mod credentials;
mod provider_manager;
mod reaper;
mod secrets;
mod server;
mod socket;

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
};

use config::LoadMode;
use dekopon_broker::{
    AuditLog, Broker, ConstraintCatalog, ConstraintSet, CredentialStore, IdentityDirectory,
    Leniency, PolicyBuildError, PolicyEngine, PolicyWorld, TraceOnlyAuditLog,
};
use dekopon_broker_host::BrokerProviderRegistry;
use dekopon_broker_protocol::ResponseEnvelope;
use dekopon_capability::EffectKind;
use dekopon_core::{AgentId, CapabilityId, Redacted, SecretDrn, error_chain};
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
    ProviderSet, ProviderSetApiVersion, ProviderStatus, ProviderSyncReport, ProviderVerifyReport,
};
pub use secrets::{
    HARD_MAX_SECRET_BYTES, HARD_MAX_SECRET_MAP_BYTES, HARD_MAX_SECRETS, SECRET_MAP_API_VERSION,
    SecretMapError, SourceError,
};
pub use server::{BrokerServer, MappedPeer, ServerError, ServerLimits};
pub use socket::{SocketError, SocketGuard, current_uid};

pub const HARD_MAX_PROVIDERS: usize = 64;

pub async fn telemetry_settings(
    config_path: impl AsRef<Path>,
    uid: u32,
) -> Result<Option<ResolvedTelemetry>, BrokerdError> {
    Ok(config::load(config_path, uid).await?.telemetry)
}

pub async fn run<F>(config_path: impl AsRef<Path>, shutdown: F) -> Result<(), BrokerdError>
where
    F: Future<Output = ()> + Send,
{
    let uid = current_uid();
    let mut config = config::load(config_path, uid).await?;
    let frame_limits = config.server_limits.frame_limits()?;
    let socket_parent = socket::validate_socket_parent(&config.socket_path, uid)?;
    // An owner-only socket under a private parent means other configured UIDs could never connect
    // — the broker starts healthy while peers loop on EACCES — so every unreachable peer is
    // checked at startup.
    if dekopon_broker_protocol::ipc_socket_mode(&socket_parent) == 0o600 {
        let configured = config
            .identities
            .iter()
            .map(|identity| identity.uid)
            .filter(|peer| *peer != uid)
            .collect::<Vec<_>>();
        if !configured.is_empty() {
            return Err(BrokerdError::UnreachablePeerUids {
                configured,
                server: uid,
            });
        }
    }
    // A compilation cache holds compiled code the broker will execute. Anyone who can write into
    // it can choose what the privileged process runs, so it must sit under a private parent.
    if let Some(cache) = &config.host_options.cwasm_dir {
        socket::validate_private_parent(cache, uid)?;
    }
    let credentials = Credentials::Loaded(match &config.credentials_path {
        Some(path) => credentials::load(path, uid).await?,
        None => CredentialStore::empty(),
    });
    let secrets = Secrets::Loaded(match &config.secret_map_path {
        Some(path) => secrets::load(path, uid).await?,
        None => dekopon_broker::SecretCatalog::empty(),
    });
    let storage = config
        .storage
        .as_ref()
        .map(|storage| {
            dekopon_storage_host::StorageHost::open(&storage.root_path, storage.limits.clone())
        })
        .transpose()
        .map_err(BrokerdError::Storage)?;
    tracing::info!(
        max_connections = config.server_limits.max_connections,
        max_memory_bytes = config.host_limits.max_memory_bytes,
        worst_case_guest_memory_bytes = config.worst_case_guest_memory_bytes,
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
        Some(config) => Some(
            assets::initialize(config)
                .await
                .map_err(BrokerdError::Assets)?,
        ),
        None => None,
    };
    let limits = ServerLimits {
        frame: frame_limits,
        max_connections: config.server_limits.max_connections,
        shutdown_grace: config.server_limits.shutdown_grace(),
    };
    let socket_path = std::mem::take(&mut config.socket_path);
    let mut warnings = Vec::new();
    let prepared = prepare(
        config,
        frame_limits.max_frame_bytes,
        Runtime {
            credentials,
            secrets,
            storage: storage.clone(),
            assets,
        },
        &mut warnings,
    )
    .await;
    for warning in &warnings {
        warning.log();
    }
    let Ready { broker, identities } = prepared.map_err(BrokerdError::from_problems)?;
    let retention = broker.storage_retention_policies();
    let server = BrokerServer::new(Arc::new(broker), identities, limits)?;
    let (listener, mut socket_guard) = socket::bind(&socket_path, uid).await?;
    tracing::info!(event = "broker_started");
    let result = reaper::serve(server.serve(listener, shutdown), storage, retention).await;

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
    Ok(())
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
}

/// Runs the startup validation `run` runs, then stops: no socket, no credentials file, no secret
/// map, no asset root and no provider storage root is touched.
pub async fn check(
    config_path: impl AsRef<Path>,
    providers: Option<CheckProviders>,
) -> CheckReport {
    let mut report = CheckReport::default();
    let uid = current_uid();
    let (mut config, refusal) = match config::load_in(config_path, uid, LoadMode::Check).await {
        Ok(loaded) => loaded,
        Err(error) => {
            report.problems.push(error.into());
            return report;
        }
    };
    report.problems.extend(refusal.map(BrokerdError::from));
    if config.identities.is_empty() {
        report.warnings.push(StartupWarning::IdentitiesRequired);
    }
    // A Check load leaves a providerSet's providers for this function to resolve.
    match (config.providers.is_empty(), providers) {
        (true, Some(providers)) => match resolve_provider_set(providers, uid).await {
            Ok(sources) => {
                config.providers = sources
                    .iter()
                    .map(|source| source.path().to_path_buf())
                    .collect();
                config.locked_providers = Some(sources);
            }
            Err(error) => {
                report.problems.push(error);
                return report;
            }
        },
        (true, None) => {
            report.problems.push(BrokerdError::ProviderSetRequired);
            return report;
        }
        (false, Some(_)) => report.problems.push(BrokerdError::ProviderSetUnused),
        (false, None) => {}
    }
    let frame_limits = match config.server_limits.frame_limits() {
        Ok(limits) => limits,
        Err(error) => {
            report.problems.push(error.into());
            return report;
        }
    };
    let scratch = match config.storage.as_ref().map(scratch_storage).transpose() {
        Ok(scratch) => scratch,
        Err(error) => {
            report.problems.push(error);
            return report;
        }
    };
    let runtime = Runtime {
        credentials: match config.credentials_path {
            Some(_) => Credentials::Unchecked,
            None => Credentials::Loaded(CredentialStore::empty()),
        },
        secrets: match config.secret_map_path {
            Some(_) => Secrets::Unchecked,
            None => Secrets::Loaded(dekopon_broker::SecretCatalog::empty()),
        },
        storage: scratch.as_ref().map(|(_, host)| host.clone()),
        assets: None,
    };
    if let Err(problems) = prepare(
        config,
        frame_limits.max_frame_bytes,
        runtime,
        &mut report.warnings,
    )
    .await
    {
        report.problems.extend(problems);
    }
    report
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
    provider_manager::load_locked_sources(&lock_file, &store, uid)
        .await
        .map_err(BrokerdError::ProviderSet)
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
    Loaded(dekopon_broker::SecretCatalog),
    Unchecked,
}

struct Runtime {
    credentials: Credentials,
    secrets: Secrets,
    storage: Option<dekopon_storage_host::StorageHost>,
    assets: Option<dekopon_http_host::asset::AssetDirectory>,
}

struct Ready {
    broker: Broker<TraceOnlyAuditLog>,
    identities: BTreeMap<u32, MappedPeer>,
}

/// Every problem found before the broker is built is returned together; the policy and the
/// constraint sets are independent, so one refusing does not hide the other.
async fn prepare(
    config: ResolvedConfig,
    max_frame_bytes: usize,
    runtime: Runtime,
    warnings: &mut Vec<StartupWarning>,
) -> Result<Ready, Vec<BrokerdError>> {
    let uid = current_uid();
    for provider in &config.providers {
        socket::validate_owned_file(provider, uid).map_err(|error| vec![error.into()])?;
    }
    let mut registry = match config.locked_providers {
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
    let (secret_catalog, secrets_unchecked) = match runtime.secrets {
        Secrets::Loaded(catalog) => (Some(catalog), false),
        Secrets::Unchecked => (None, true),
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
    // The revision stamped on receipts and audit records is the policy set's own fingerprint, so
    // it moves exactly when authorization can.
    let revision = policy.digest().to_owned();
    let leniency = if config.strict {
        Leniency::Strict
    } else {
        Leniency::Tolerant
    };
    let (broker, broker_warnings) = Broker::start(
        registry,
        BROKER_PRINCIPAL
            .parse()
            .expect("the broker principal is a valid identifier"),
        revision,
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
    Ok(Ready { broker, identities })
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
        let (capabilities, command_words) = broker.capability_view(&peer.context);
        let response = ResponseEnvelope::capabilities(capabilities, command_words);
        let length = encoded_capability_response(&response)?;
        if length > maximum {
            return Err(BrokerdError::CapabilityResponseTooLarge { length, maximum });
        }
    }
    // Direct peers are granted almost nothing in a gateway deployment; the real capability sets are
    // reachable only through attested sessions that can't be enumerated at startup, so the broker
    // bounds their worst case instead.
    let (capabilities, command_words) = broker.capability_ceiling();
    let response = ResponseEnvelope::chat_capabilities(
        capabilities,
        command_words,
        broker.chat_memory_ceiling(),
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
    #[error("the configuration names a providerSet; check it with --provider-set and --store")]
    ProviderSetRequired,
    #[error("the configuration names provider paths; --provider-set does not apply to it")]
    ProviderSetUnused,
    #[error("the provider set could not be resolved")]
    ProviderSet(#[source] ProviderManagerError),
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
