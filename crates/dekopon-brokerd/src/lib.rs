//! Authenticated, deny-by-default local Unix broker service.
//!
//! The server derives caller identity exclusively from Unix peer credentials and a trusted
//! owner-controlled configuration. Wire payloads remain untrusted invocation proposals.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod checkpoint;
mod config;
mod credentials;
mod server;
mod socket;

use std::{
    collections::BTreeMap,
    env,
    future::{Future, pending},
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::Duration,
};

use dekopon_broker::{
    AuditLog, Broker, ConstraintCatalog, CredentialStore, FileAuditLog, IdentityDirectory,
    Leniency, PolicyBuildError, PolicyEngine, PolicyWorld,
};
use dekopon_broker_host::BrokerProviderRegistry;
use dekopon_broker_protocol::ResponseEnvelope;
use thiserror::Error;

pub use checkpoint::{CHECKPOINT_API_VERSION, CheckpointError, HARD_MAX_CHECKPOINT_BYTES};
pub use config::{
    BrokerdConfig, CONFIG_API_VERSION, ConfigApiVersion, ConfigError, HostLimitsConfig,
    IdentityMapping, PeerIdentity, ResolvedConfig, ResolvedTelemetry, ServerLimitsConfig,
    StorageConfig, TelemetryConfig,
};
pub use credentials::{
    CREDENTIALS_API_VERSION, CredentialsError, HARD_MAX_CREDENTIALS, HARD_MAX_CREDENTIALS_BYTES,
};
pub use server::{BrokerServer, MappedPeer, ServerError, ServerLimits};
pub use socket::{SocketError, SocketGuard, current_uid};

/// Verified durable chain state at clean shutdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditCheckpoint {
    /// Number of retained records.
    pub records: usize,
    /// Current hash-chain head, absent only for an empty log.
    pub head: Option<String>,
}

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
///
/// This compatibility entry point does not open the informational HTTP listener. Use
/// [`run_with_http`] to enable the web UI explicitly.
pub async fn run<F>(
    config_path: impl AsRef<Path>,
    shutdown: F,
) -> Result<AuditCheckpoint, BrokerdError>
where
    F: Future<Output = ()> + Send,
{
    run_with_http(config_path, None, shutdown).await
}

/// Loads trusted configuration, builds the privileged host, and optionally serves the read-only
/// web UI on `http_bind` until shutdown.
pub async fn run_with_http<F>(
    config_path: impl AsRef<Path>,
    http_bind: Option<SocketAddr>,
    shutdown: F,
) -> Result<AuditCheckpoint, BrokerdError>
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
    for identity in &config.identities {
        if identity.uid != uid {
            return Err(BrokerdError::UnreachablePeerUid {
                configured: identity.uid,
                server: uid,
            });
        }
    }
    socket::validate_private_parent(&config.socket_path, uid)?;
    socket::validate_private_parent(&config.audit_path, uid)?;
    socket::validate_private_parent(&config.checkpoint_path, uid)?;
    socket::validate_private_parent(&config.checkpoint_lock_path, uid)?;
    for provider in &config.providers {
        socket::validate_owned_file(provider, uid)?;
    }
    // Loaded before the policy is built so an unknown or unbindable credential is a startup
    // refusal, never a per-invocation surprise. Absent path ⇒ empty store ⇒ credentialed
    // constraint sets fail construction the same way.
    let credential_store = match &config.credentials_path {
        Some(path) => credentials::load(path, uid).await?,
        None => CredentialStore::empty(),
    };

    let (checkpoint_store, stored_checkpoint) = checkpoint::CheckpointStore::open(
        &config.checkpoint_path,
        &config.checkpoint_lock_path,
        uid,
    )
    .await
    .map_err(BrokerdError::Checkpoint)?;
    let checkpoint_store = Arc::new(checkpoint_store);
    let file_audit = Arc::new(
        FileAuditLog::open(
            &config.audit_path,
            config.server_limits.audit_max_records,
            config.server_limits.audit_max_line_bytes,
        )
        .await
        .map_err(BrokerdError::Audit)?,
    );
    socket::validate_owned_file(&config.audit_path, uid)?;
    checkpoint::reconcile(&file_audit, &checkpoint_store, stored_checkpoint.as_ref())
        .await
        .map_err(BrokerdError::Checkpoint)?;
    let replay_ids = file_audit.replay_ids().await;
    let audit = Arc::new(checkpoint::CheckpointedAuditLog::new(
        file_audit,
        checkpoint_store,
    ));
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
    let storage_gc = storage_host.clone();
    let storage_gc_interval = config
        .storage
        .as_ref()
        .map(|storage| Duration::from_millis(storage.limits.gc_interval_ms));
    let registry = BrokerProviderRegistry::load_with_storage(
        config.providers,
        config.host_limits,
        storage_host,
    )
    .await
    .map_err(BrokerdError::Host)?;
    let host_metrics = registry.metrics();
    let provider_metadata = registry.loaded_provider_metadata().collect::<Vec<_>>();
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
        replay_ids,
    )
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
    let service_status = dekopon_webui::ServiceStatus::default();
    let web_shutdown_grace = limits.shutdown_grace;
    let server = BrokerServer::new_with_status(broker, identities, limits, service_status.clone())?;
    let dashboard = dekopon_webui::Dashboard::new(
        env!("CARGO_PKG_VERSION"),
        provider_metadata,
        host_metrics,
        service_status,
        webui_otel_summary(config.telemetry.as_ref()),
    );
    let web_listener = match http_bind {
        Some(address) => Some(
            tokio::net::TcpListener::bind(address)
                .await
                .map_err(|source| BrokerdError::WebUiBind { address, source })?,
        ),
        None => None,
    };
    let web_address = web_listener
        .as_ref()
        .map(tokio::net::TcpListener::local_addr)
        .transpose()
        .map_err(|source| BrokerdError::WebUiAddress { source })?;
    let web_enabled = web_listener.is_some();
    let (listener, mut socket_guard) = socket::bind(&config.socket_path, uid).await?;
    let (records, head) = audit.checkpoint().await;
    tracing::info!(
        event = "broker_started",
        audit_records = records,
        audit_head = head.as_deref().unwrap_or("none")
    );
    if let Some(address) = web_address {
        tracing::info!(
            event = "broker_webui_started",
            http.bind = %address,
            http.path = "/ui",
            authentication = "none"
        );
    }

    let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
    let broker_serve = server.serve(listener, wait_for_shutdown(shutdown_receiver.clone()));
    let storage_gc_serve =
        storage_gc_loop(storage_gc, storage_gc_interval, shutdown_receiver.clone());
    let web_serve = async move {
        match web_listener {
            Some(listener) => {
                dekopon_webui::serve(listener, dashboard, wait_for_shutdown(shutdown_receiver))
                    .await
            }
            None => pending::<Result<(), dekopon_webui::WebUiError>>().await,
        }
    };
    tokio::pin!(broker_serve);
    tokio::pin!(storage_gc_serve);
    tokio::pin!(web_serve);
    tokio::pin!(shutdown);

    let mut broker_result = None;
    let mut web_result = None;
    tokio::select! {
        () = &mut shutdown => {}
        result = &mut broker_serve => broker_result = Some(result),
        result = &mut web_serve => web_result = Some(result),
    }
    #[allow(
        clippy::let_underscore_must_use,
        reason = "SendError here means every serve task already ended, which is the outcome this broadcast asks for"
    )]
    let _ = shutdown_sender.send(true);
    if broker_result.is_none() {
        broker_result = Some(broker_serve.await);
    }
    if tokio::time::timeout(web_shutdown_grace, &mut storage_gc_serve)
        .await
        .is_err()
    {
        return Err(BrokerdError::StorageGcShutdownTimeout);
    }
    let mut web_shutdown_timed_out = false;
    if web_enabled && web_result.is_none() {
        match tokio::time::timeout(web_shutdown_grace, &mut web_serve).await {
            Ok(result) => web_result = Some(result),
            Err(_) => web_shutdown_timed_out = true,
        }
    }

    socket_guard.cleanup()?;
    broker_result.ok_or(BrokerdError::Server(ServerError::ConnectionTask))??;
    if web_shutdown_timed_out {
        return Err(BrokerdError::WebUiShutdownTimeout);
    }
    if let Some(result) = web_result {
        result.map_err(BrokerdError::WebUi)?;
        tracing::info!(event = "broker_webui_stopped");
    }
    let (records, head) = audit.checkpoint().await;
    tracing::info!(
        event = "broker_stopped",
        audit_records = records,
        audit_head = head.as_deref().unwrap_or("none")
    );
    Ok(AuditCheckpoint { records, head })
}

async fn storage_gc_loop(
    host: Option<dekopon_storage_host::StorageHost>,
    interval: Option<Duration>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let (Some(host), Some(interval)) = (host, interval) else {
        wait_for_shutdown(shutdown).await;
        return;
    };
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {
                let host = host.clone();
                match tokio::task::spawn_blocking(move || host.gc_once()).await {
                    Ok(Ok(report)) => tracing::debug!(
                        event = "broker_storage_gc_completed",
                        namespace.count = report.namespaces_removed,
                        storage.byte_bucket = if report.bytes_removed == 0 { 0 } else { 64 - report.bytes_removed.leading_zeros() },
                    ),
                    Ok(Err(_)) | Err(_) => tracing::warn!(
                        event = "broker_storage_gc_failed",
                        category = "storage",
                    ),
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return; }
            }
        }
    }
}

async fn wait_for_shutdown(mut receiver: tokio::sync::watch::Receiver<bool>) {
    if *receiver.borrow() {
        return;
    }
    while receiver.changed().await.is_ok() {
        if *receiver.borrow() {
            return;
        }
    }
}

fn webui_otel_summary(telemetry: Option<&ResolvedTelemetry>) -> Option<dekopon_webui::OtelSummary> {
    let telemetry = telemetry?;
    let headers_configured = [
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    ]
    .into_iter()
    .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()));
    Some(dekopon_webui::OtelSummary {
        endpoint: telemetry.settings.endpoint().to_owned(),
        transport: telemetry.settings.transport().to_string(),
        service_name: telemetry.settings.service_name().to_owned(),
        export_timeout_ms: u64::try_from(telemetry.settings.timeout().as_millis())
            .unwrap_or(u64::MAX),
        telemetry_payloads: telemetry.telemetry_payloads,
        headers_configured,
        resource_attributes_configured: env::var_os("OTEL_RESOURCE_ATTRIBUTES")
            .is_some_and(|value| !value.is_empty()),
    })
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
        let response = ResponseEnvelope::capabilities(
            broker.capabilities(&peer.context),
            broker.command_words(&peer.context),
        );
        let length = serde_json::to_vec(&response)
            .map_err(|source| BrokerdError::CapabilityResponse { source })?
            .len();
        if length > maximum {
            return Err(BrokerdError::CapabilityResponseTooLarge { length, maximum });
        }
    }
    Ok(())
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
    /// Owner-only durable audit could not be opened and verified.
    #[error("broker durable audit is unavailable")]
    Audit(#[source] dekopon_broker::FileAuditError),
    /// Durable checkpoint could not be locked, verified, reconciled, or synchronized.
    #[error("broker audit checkpoint is unavailable")]
    Checkpoint(#[source] CheckpointError),
    /// Provider storage root/key/recovery could not start.
    #[error("broker provider storage could not start")]
    Storage(#[source] dekopon_storage_host::StorageHostError),
    /// A blocking provider-storage GC pass did not drain inside shutdown grace.
    #[error("broker provider storage GC did not stop inside shutdown grace")]
    StorageGcShutdownTimeout,
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
    /// Policy, restored replay state, or constraints were invalid.
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
    /// Owner-only socket permissions make a different UID unreachable.
    #[error(
        "configured peer UID {configured} cannot reach owner-only socket for server UID {server}"
    )]
    UnreachablePeerUid { configured: u32, server: u32 },
    /// Listener serving or bounded shutdown failed.
    #[error("broker server failed")]
    Server(#[from] ServerError),
    /// The explicitly requested informational HTTP address could not be bound.
    #[error("could not bind Dekopon web UI to {address}")]
    WebUiBind {
        /// Requested TCP address.
        address: SocketAddr,
        /// Bind failure.
        #[source]
        source: std::io::Error,
    },
    /// The bound informational listener's local address could not be inspected.
    #[error("could not inspect Dekopon web UI listener address")]
    WebUiAddress {
        /// Socket failure.
        #[source]
        source: std::io::Error,
    },
    /// The informational HTTP server failed while the broker was running.
    #[error("Dekopon web UI failed")]
    WebUi(#[source] dekopon_webui::WebUiError),
    /// Open informational HTTP connections did not close inside the broker shutdown grace.
    #[error("Dekopon web UI did not stop inside the configured shutdown grace")]
    WebUiShutdownTimeout,
}

#[cfg(test)]
mod tests;
