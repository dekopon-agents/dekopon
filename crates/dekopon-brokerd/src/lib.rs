//! Authenticated, deny-by-default local Unix broker service.
//!
//! The server derives caller identity exclusively from Unix peer credentials and a trusted
//! owner-controlled configuration. Wire payloads remain untrusted invocation proposals.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod config;
mod server;
mod socket;

use std::{collections::BTreeMap, future::Future, path::Path, sync::Arc};

use dekopon_broker::{Broker, FileAuditLog};
use dekopon_broker_host::BrokerProviderRegistry;
use dekopon_broker_protocol::ResponseEnvelope;
use thiserror::Error;

pub use config::{
    BrokerdConfig, CONFIG_API_VERSION, ConfigApiVersion, ConfigError, HostLimitsConfig,
    PeerIdentity, ResolvedConfig, ServerLimitsConfig,
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
    for provider in &config.providers {
        socket::validate_owned_file(provider, uid)?;
    }

    let audit = Arc::new(
        FileAuditLog::open(
            &config.audit_path,
            config.server_limits.audit_max_records,
            config.server_limits.audit_max_line_bytes,
        )
        .await
        .map_err(BrokerdError::Audit)?,
    );
    socket::validate_owned_file(&config.audit_path, uid)?;
    let replay_ids = audit.replay_ids().await;
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

fn validate_capability_responses(
    broker: &Broker<FileAuditLog>,
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
