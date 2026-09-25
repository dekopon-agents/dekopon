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
mod config;
mod credentials;
mod provider_manager;
mod secrets;
mod server;
mod socket;

use std::{collections::BTreeMap, future::Future, path::Path, sync::Arc};

use dekopon_broker::{
    AuditLog, Broker, ConstraintCatalog, CredentialStore, IdentityDirectory, Leniency,
    PolicyBuildError, PolicyEngine, PolicyWorld, TraceOnlyAuditLog,
};
use dekopon_broker_host::BrokerProviderRegistry;
use dekopon_broker_protocol::ResponseEnvelope;
use dekopon_capability::EffectKind;
use dekopon_core::error_chain;
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
    let config = config::load(config_path, uid).await?;
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
    for provider in &config.providers {
        socket::validate_owned_file(provider, uid)?;
    }
    // A compilation cache holds compiled code the broker will execute. Anyone who can write into
    // it can choose what the privileged process runs, so it must sit under a private parent.
    if let Some(cache) = &config.host_options.cwasm_dir {
        socket::validate_private_parent(cache, uid)?;
    }
    let credential_store = match &config.credentials_path {
        Some(path) => credentials::load(path, uid).await?,
        None => CredentialStore::empty(),
    };
    let secret_catalog = match &config.secret_map_path {
        Some(path) => secrets::load(path, uid).await?,
        None => dekopon_broker::SecretCatalog::empty(),
    };
    let secret_drns = secret_catalog.drns().cloned().collect::<Vec<_>>();

    // The audit record is the log event the broker emits inside the trace: stdout JSON always,
    // an OTLP log record once `telemetry` names a receiver. Nothing else keeps a copy.
    let audit = Arc::new(TraceOnlyAuditLog);
    let storage_host = config
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
    let asset_directory = match &config.assets {
        Some(config) => Some(
            assets::initialize(config)
                .await
                .map_err(BrokerdError::Assets)?,
        ),
        None => None,
    };
    let mut registry = match config.locked_providers {
        Some(sources) => {
            BrokerProviderRegistry::load_locked_with_options(
                sources,
                config.host_limits,
                storage_host,
                &config.host_options,
            )
            .await
        }
        None => {
            BrokerProviderRegistry::load_with_options(
                config.providers,
                config.host_limits,
                storage_host,
                &config.host_options,
            )
            .await
        }
    }
    .map_err(BrokerdError::Host)?;
    if let Some(directory) = asset_directory {
        registry.set_assets(directory);
    }
    validate_manifest_metadata(
        &registry,
        frame_limits
            .max_frame_bytes
            .saturating_sub(config::MINIMUM_RESPONSE_OVERHEAD_BYTES),
    )?;
    let identity_directory =
        IdentityDirectory::new(config.principals.iter().flat_map(|(principal, entry)| {
            entry
                .subjects
                .iter()
                .map(|subject| (subject.clone(), principal.clone()))
        }))
        .map_err(BrokerdError::Broker)?;
    let world = PolicyWorld::new(
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
            .with_secrets(secret_drns)
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
    .map_err(|source| BrokerdError::Policy { source })?;
    let leniency = if config.strict {
        Leniency::Strict
    } else {
        Leniency::Tolerant
    };
    let policy = if config.strict {
        PolicyEngine::new(&config.policies, &world)
            .map_err(|source| BrokerdError::Policy { source })?
    } else {
        let (policy, unresolved) = PolicyEngine::new_lenient(&config.policies, &world)
            .map_err(|source| BrokerdError::Policy { source })?;
        for entry in &unresolved {
            tracing::warn!(
                target: "dekopon_brokerd::audit",
                {
                    audit.event = "policy.name.unresolved",
                    policy.id = %entry.policy,
                    name.kind = entry.kind.label(),
                    name = %entry.name,
                },
                "policy names {} {:?}, which no loaded provider declares; it can never match",
                entry.kind.label(),
                entry.name
            );
        }
        policy
    };
    let (constraint_sets, problems) =
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
    let (tolerable, fatal): (Vec<_>, Vec<_>) = problems
        .into_iter()
        .partition(|problem| problem.is_unloaded_name() && !config.strict);
    if !fatal.is_empty() {
        return Err(BrokerdError::Capabilities { problems: fatal });
    }
    for problem in &tolerable {
        tracing::warn!(
            target: "dekopon_brokerd::audit",
            { audit.event = "config.startup.warning", reason = "unloaded-capability" },
            "{problem}"
        );
    }
    let constraints = ConstraintCatalog::new(constraint_sets)
        .map_err(BrokerdError::Broker)?
        .with_agent_credentials(
            config
                .agents
                .into_iter()
                .map(|(agent, binding)| (agent, binding.credentials))
                .collect(),
        );
    let (broker, warnings) = Broker::start(
        registry,
        config.broker_principal,
        config.policy_revision,
        policy,
        constraints,
        credential_store,
        identity_directory,
        Arc::clone(&audit),
        config.broker_limits,
        leniency,
    )
    .map_err(BrokerdError::Broker)?;
    let broker = broker
        .with_secret_catalog(secret_catalog)
        .map_err(BrokerdError::Broker)?;
    let broker = match config.chat_memory {
        Some(memory) => broker
            .with_chat_memory(memory)
            .map_err(BrokerdError::Broker)?,
        None => broker,
    };
    for warning in &warnings {
        tracing::warn!(
            target: "dekopon_brokerd::audit",
            {
                audit.event = "config.startup.warning",
                reason = warning.reason(),
                capability.id = %warning.capability(),
            },
            "{warning}"
        );
    }
    let broker = Arc::new(broker);
    let mut identities = BTreeMap::new();
    for identity in config.identities {
        identities.insert(
            identity.uid,
            MappedPeer {
                context: identity.context().map_err(BrokerdError::Context)?,
                attestor: identity.attestor,
            },
        );
    }
    validate_capability_responses(&broker, &identities, frame_limits.max_frame_bytes)?;
    let limits = ServerLimits {
        frame: frame_limits,
        max_connections: config.server_limits.max_connections,
        shutdown_grace: config.server_limits.shutdown_grace(),
    };
    let server = BrokerServer::new(broker, identities, limits)?;
    let (listener, mut socket_guard) = socket::bind(&config.socket_path, uid).await?;
    tracing::info!(event = "broker_started");
    let result = server.serve(listener, shutdown).await;

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
