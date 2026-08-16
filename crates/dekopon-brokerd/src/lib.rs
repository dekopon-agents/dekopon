//! Authenticated, deny-by-default local Unix broker service.
//!
//! The server derives caller identity exclusively from Unix peer credentials and a trusted
//! owner-controlled configuration. Wire payloads remain untrusted invocation proposals.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod checkpoint;
mod config;
mod server;
mod socket;

use std::{collections::BTreeMap, future::Future, path::Path, sync::Arc};

use dekopon_broker::{AuditLog, Broker, FileAuditLog};
use dekopon_broker_host::BrokerProviderRegistry;
use dekopon_broker_protocol::ResponseEnvelope;
use thiserror::Error;

pub use checkpoint::{CHECKPOINT_API_VERSION, CheckpointError, HARD_MAX_CHECKPOINT_BYTES};
pub use config::{
    BrokerdConfig, CONFIG_API_VERSION, ConfigApiVersion, ConfigError, HostLimitsConfig,
    PeerIdentity, ResolvedConfig, ResolvedTelemetry, ServerLimitsConfig, TelemetryConfig,
};
pub use server::{BrokerServer, ServerError, ServerLimits};
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
pub async fn run<F>(
    config_path: impl AsRef<Path>,
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
    let registry = BrokerProviderRegistry::load(config.providers, config.host_limits)
        .await
        .map_err(BrokerdError::Host)?;
    validate_manifest_metadata(
        &registry,
        frame_limits
            .max_frame_bytes
            .saturating_sub(config::MINIMUM_RESPONSE_OVERHEAD_BYTES),
    )?;
    let broker = Arc::new(
        Broker::new_with_replay_ids(
            registry,
            config.broker_principal,
            config.policy_revision,
            config.rules,
            Arc::clone(&audit),
            config.broker_limits,
            replay_ids,
        )
        .map_err(BrokerdError::Broker)?,
    );
    let mut identities = BTreeMap::new();
    for identity in config.identities {
        identities.insert(
            identity.uid,
            identity.context().map_err(BrokerdError::Context)?,
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
    let (records, head) = audit.checkpoint().await;
    tracing::info!(
        event = "broker_started",
        audit_records = records,
        audit_head = head.as_deref().unwrap_or("none")
    );
    let serve_result = server.serve(listener, shutdown).await;
    socket_guard.cleanup()?;
    serve_result?;
    let (records, head) = audit.checkpoint().await;
    tracing::info!(
        event = "broker_stopped",
        audit_records = records,
        audit_head = head.as_deref().unwrap_or("none")
    );
    Ok(AuditCheckpoint { records, head })
}

fn validate_capability_responses<A: AuditLog>(
    broker: &Broker<A>,
    identities: &BTreeMap<u32, dekopon_broker::AuthenticatedContext>,
    maximum: usize,
) -> Result<(), BrokerdError> {
    for context in identities.values() {
        let response = ResponseEnvelope::capabilities(broker.capabilities(context));
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
    /// Owner-only durable audit could not be opened and verified.
    #[error("broker durable audit is unavailable")]
    Audit(#[source] dekopon_broker::FileAuditError),
    /// Durable checkpoint could not be locked, verified, reconciled, or synchronized.
    #[error("broker audit checkpoint is unavailable")]
    Checkpoint(#[source] CheckpointError),
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
}

#[cfg(test)]
mod tests;
