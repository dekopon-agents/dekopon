//! Authenticated, deny-by-default local Unix broker service.
//!
//! The server derives caller identity exclusively from Unix peer credentials and a trusted
//! owner-controlled configuration. Wire payloads remain untrusted invocation proposals.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod config;
mod credentials;
mod provider_manager;
mod secrets;
mod server;
mod socket;

use std::{collections::BTreeMap, future::Future, path::Path, sync::Arc};

use dekopon_broker::{
    AuditLog, Broker, ConstraintCatalog, CredentialStore, FileAuditLog, IdentityDirectory,
    Leniency, PolicyBuildError, PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::BrokerProviderRegistry;
use dekopon_broker_protocol::ResponseEnvelope;
use dekopon_core::error_chain;
use thiserror::Error;

pub use config::{
    BrokerdConfig, CONFIG_API_VERSION, ConfigApiVersion, ConfigError, HostLimitsConfig,
    IdentityMapping, ManagedProviderSetConfig, PeerIdentity, ResolvedConfig, ResolvedTelemetry,
    ServerLimitsConfig, StorageConfig, TelemetryConfig,
};
pub use credentials::{
    CREDENTIALS_API_VERSION, CredentialsError, HARD_MAX_CREDENTIALS, HARD_MAX_CREDENTIALS_BYTES,
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

/// Maximum provider components in either legacy configuration or a managed lock.
pub const HARD_MAX_PROVIDERS: usize = 64;

/// Reads only the export settings, so the process can install its subscriber before serving.
///
/// The configuration is parsed again by [`run`], which reports every configuration failure with
/// full context. Re-reading a bounded owner-only file is cheaper and clearer than threading a
/// subscriber handle through the service, and this call decides one thing: whether an OTLP layer
/// is installed at all.
///
/// # Errors
///
/// Returns the same configuration errors [`run`] would, so a caller that wants to fail fast may
/// surface them; callers that prefer `run`'s reporting can discard this error.
pub async fn telemetry_settings(
    config_path: impl AsRef<Path>,
    uid: u32,
) -> Result<Option<ResolvedTelemetry>, BrokerdError> {
    Ok(config::load(config_path, uid).await?.telemetry)
}

/// Loads trusted configuration, builds the privileged host, and serves until shutdown.
pub async fn run<F>(config_path: impl AsRef<Path>, shutdown: F) -> Result<(), BrokerdError>
where
    F: Future<Output = ()> + Send,
{
    let uid = current_uid();
    let config = config::load(config_path, uid).await?;
    // Span verbosity is process state rather than a parameter because it describes the deployment,
    // not the call. Set before serving so no invocation is recorded under the wrong mode.
    dekopon_core::set_telemetry_payloads(
        config
            .telemetry
            .as_ref()
            .is_some_and(|telemetry| telemetry.telemetry_payloads),
    );
    let frame_limits = config.server_limits.frame_limits()?;
    socket::validate_socket_parent(&config.socket_path, uid)?;
    socket::validate_private_parent(&config.audit_path, uid)?;
    for provider in &config.providers {
        socket::validate_owned_file(provider, uid)?;
    }
    // A compilation cache holds compiled code the broker will execute. Anyone who can write into
    // it can choose what the privileged process runs, so it lives under the same private-parent
    // rule as the audit log.
    if let Some(cache) = &config.host_options.compile_cache_dir {
        socket::validate_private_parent(cache, uid)?;
    }
    // Loaded before the policy is built so an unknown or unbindable credential is a startup
    // refusal, never a per-invocation surprise. Absent path ⇒ empty store ⇒ credentialed
    // constraint sets fail construction the same way.
    let credential_store = match &config.credentials_path {
        Some(path) => credentials::load(path, uid).await?,
        None => CredentialStore::empty(),
    };
    let secret_catalog = match &config.secret_map_path {
        Some(path) => secrets::load(path, uid).await?,
        None => dekopon_broker::SecretCatalog::empty(),
    };
    let secret_drns = secret_catalog.drns().cloned().collect::<Vec<_>>();

    let file_audit = Arc::new(
        FileAuditLog::open(
            &config.audit_path,
            config.server_limits.audit_max_line_bytes,
        )
        .await
        .map_err(BrokerdError::Audit)?,
    );
    socket::validate_owned_file(&config.audit_path, uid)?;
    let audit = file_audit;
    let storage_host = config
        .storage
        .as_ref()
        .map(|storage| {
            dekopon_storage_host::StorageHost::open(
                &storage.root_path,
                &storage.namespace_key_path,
                storage.limits.clone(),
            )
        })
        .transpose()
        .map_err(BrokerdError::Storage)?;
    // Stated once at startup because nothing else in the process can: per-store limits are visible
    // in the host stats, but the product with the connection ceiling is what a container limit has
    // to cover, and an unbounded aggregate is a deliberate operator choice rather than a default.
    tracing::info!(
        max_connections = config.server_limits.max_connections,
        max_memory_bytes = config.host_limits.max_memory_bytes,
        worst_case_guest_memory_bytes = config.worst_case_guest_memory_bytes,
        aggregate_ceiling_bytes = config.host_options.max_total_memory_bytes,
        compile_cache = config
            .host_options
            .compile_cache_dir
            .as_ref()
            .map(|path| path.display().to_string()),
        "broker provider guest-memory budget"
    );
    let registry = match config.locked_providers {
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
    validate_manifest_metadata(
        &registry,
        frame_limits
            .max_frame_bytes
            .saturating_sub(config::MINIMUM_RESPONSE_OVERHEAD_BYTES),
    )?;
    let identity_directory = IdentityDirectory::new(
        config
            .identity_mappings
            .iter()
            .map(|mapping| (mapping.subject.clone(), mapping.principal.clone())),
    )
    .map_err(BrokerdError::Broker)?;
    // The declared world is exactly what owner-controlled configuration names: the peers that can
    // connect, the principals subjects map to, and the capabilities the loaded manifests expose.
    //
    // What happens when configuration names something outside it depends on `strict`. Strict
    // refuses to start, which is the right posture for a deployment whose provider set is fixed.
    // The default tolerates it and warns, so an operator can ship policy and constraint sets that
    // anticipate a provider they have not dropped in yet. Tolerating grants nothing: an
    // anticipated capability routes nowhere, so every invocation of it is denied
    // `unconstrained-capability` before Cedar is consulted.
    let world = PolicyWorld::new(
        config
            .identities
            .iter()
            .map(|identity| identity.principal.clone())
            .chain(
                config
                    .identity_mappings
                    .iter()
                    .map(|mapping| mapping.principal.clone()),
            ),
        registry
            .capabilities()
            .map(|(provider, capability)| (capability.id.clone(), provider.clone())),
    )
    .map(|world| world.with_secrets(secret_drns))
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
    let constraints =
        ConstraintCatalog::new(config.constraint_sets).map_err(BrokerdError::Broker)?;
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

    // The socket must not outlive its listener, so cleanup still runs here — but its result is
    // held rather than returned. A stale socket path is a smaller problem than the failure that
    // ended service, and returning it first would replace the real cause and skip the final
    // `broker_stopped` entirely.
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
        // Command words ride in this response, so they count toward the frame bound. Leaving them
        // out would let a provider directory with a large vocabulary pass startup and then fail to
        // serve the very first session.
        let (capabilities, command_words) = broker.capability_view(&peer.context);
        let response = ResponseEnvelope::capabilities(capabilities, command_words);
        let length = encoded_capability_response(&response)?;
        if length > maximum {
            return Err(BrokerdError::CapabilityResponseTooLarge { length, maximum });
        }
    }
    // The peers above are the *direct* callers, and in a gateway deployment they are the ones
    // granted almost nothing. Every chat session is answered through an attested `capabilities`
    // under a context built from an identity mapping, whose Cedar grants are the real, larger
    // capability sets — so checking peers alone checks the one path that never carries the big
    // response, and the oversized one still fails `write_frame` on every session open. That is
    // exactly the failure this check exists to move to startup.
    //
    // Those contexts cannot be enumerated here: the agent catalog belongs to the gateway and
    // production policy conditions on `context.agent`, so a representative agent would measure a
    // surface no session receives. The broker bounds them instead, and a ceiling that fits proves
    // every session's answer fits.
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

/// Secure startup, execution, or shutdown failure.
#[derive(Debug, Error)]
pub enum BrokerdError {
    /// Filesystem or socket validation failed.
    #[error("broker socket security validation failed")]
    Socket(#[from] SocketError),
    /// Strict owner-controlled configuration failed.
    #[error("broker configuration is invalid")]
    Config(#[from] ConfigError),
    /// Owner-only credential storage failed hygiene, decoding, or resolution.
    #[error("broker credentials are unavailable or invalid")]
    Credentials(#[from] CredentialsError),
    /// Owner-only public-DRN to private-source map failed validation.
    #[error("broker private secret map is unavailable or invalid")]
    Secrets(#[from] SecretMapError),
    /// Owner-only audit could not be opened.
    #[error("broker durable audit is unavailable")]
    Audit(#[source] dekopon_broker::FileAuditError),
    /// Provider storage root/key validation could not start.
    #[error("broker provider storage could not start")]
    Storage(#[source] dekopon_storage_host::StorageHostError),
    /// Provider components could not be validated and compiled.
    #[error("broker provider host could not start")]
    Host(#[source] dekopon_broker_host::BrokerHostError),
    /// Validated manifest metadata could not be encoded.
    #[error("broker provider metadata could not be encoded")]
    ManifestMetadata {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Aggregate provider metadata could not fit a bounded capability response.
    #[error("broker provider metadata is {length} bytes; maximum is {maximum}")]
    ManifestMetadataTooLarge {
        /// Encoded aggregate length.
        length: usize,
        /// Maximum reserved metadata bytes.
        maximum: usize,
    },
    /// A mapped capability response could not be encoded.
    #[error("broker capability response could not be encoded")]
    CapabilityResponse {
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// A mapped capability response exceeded the configured frame.
    #[error("broker capability response is {length} bytes; frame maximum is {maximum}")]
    CapabilityResponseTooLarge {
        /// Encoded response length.
        length: usize,
        /// Configured frame maximum.
        maximum: usize,
    },
    /// The widest capability response an attested session could receive exceeded the frame.
    #[error(
        "broker could answer a session with a {length}-byte capability response; frame maximum is {maximum}"
    )]
    CapabilityCeilingTooLarge {
        /// Encoded length of the widest possible response.
        length: usize,
        /// Configured frame maximum.
        maximum: usize,
    },
    /// Policy or constraints were invalid.
    #[error("broker policy could not start")]
    Broker(#[source] dekopon_broker::BrokerBuildError),
    /// The Cedar policy set could not be parsed, schema-validated, or bounded.
    #[error("broker policy set is invalid")]
    Policy {
        /// Policy build failure.
        #[source]
        source: PolicyBuildError,
    },
    /// A configured transport identity could not be bound.
    #[error("broker peer identity is invalid")]
    Context(#[source] dekopon_broker::ContextError),
    /// Listener serving or bounded shutdown failed.
    #[error("broker server failed")]
    Server(#[from] ServerError),
}

#[cfg(test)]
mod tests;
