use std::{collections::BTreeMap, future::Future, io, sync::Arc, time::Duration};

use dekopon_broker::{AttestorGrant, AuditLog, AuthenticatedContext, Broker, BrokerError};
use dekopon_broker_host::CommandResolution;
use dekopon_broker_protocol::{
    BrokerRequest, ERROR_BROKER_UNAVAILABLE, ERROR_INVALID_REQUEST, ERROR_OUTCOME_UNAUDITED,
    ERROR_PROVIDER, ERROR_UNAUTHENTICATED, FrameLimits, ProtocolError, RequestEnvelope,
    ResponseEnvelope, read_frame, write_frame,
};
use dekopon_core::{InvocationId, TraceId};
use dekopon_telemetry::TraceContextParts;
use dekopon_webui::ServiceStatus;
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::config::HARD_MAX_CONNECTIONS;

pub(crate) fn storage_invocation_span(invocation: &InvocationId, trace: &TraceId) -> tracing::Span {
    tracing::info_span!("broker.invocation", invocation = %invocation, trace = %trace)
}

/// One mapped peer: its trusted transport context and optional attestor authority.
///
/// The grant lives beside the context rather than inside it because it is authority *about
/// identity derivation*, not identity: a peer with a grant still acts as itself on direct
/// operations, and only `invokeFor`/`capabilitiesFor` consult the grant at all.
#[derive(Clone, Debug)]
pub struct MappedPeer {
    /// The peer's own authenticated context.
    pub context: AuthenticatedContext,
    /// The peer's owner-configured attestor authority, when it has any.
    pub attestor: Option<AttestorGrant>,
}

#[derive(Clone, Copy, Debug)]
pub struct ServerLimits {
    pub frame: FrameLimits,
    pub max_connections: usize,
    pub shutdown_grace: Duration,
}

pub struct BrokerServer<A>
where
    A: AuditLog,
{
    broker: Arc<Broker<A>>,
    identities: Arc<BTreeMap<u32, MappedPeer>>,
    status: ServiceStatus,
    limits: ServerLimits,
}

impl<A> BrokerServer<A>
where
    A: AuditLog + 'static,
{
    pub fn new(
        broker: Arc<Broker<A>>,
        identities: BTreeMap<u32, MappedPeer>,
        limits: ServerLimits,
    ) -> Result<Self, ServerError> {
        Self::new_with_status(broker, identities, limits, ServiceStatus::default())
    }

    /// Builds a server whose informational reports feed the supplied web-UI state.
    pub fn new_with_status(
        broker: Arc<Broker<A>>,
        identities: BTreeMap<u32, MappedPeer>,
        limits: ServerLimits,
        status: ServiceStatus,
    ) -> Result<Self, ServerError> {
        limits
            .frame
            .validate()
            .map_err(|_| ServerError::InvalidLimits)?;
        if limits.max_connections == 0
            || limits.max_connections > HARD_MAX_CONNECTIONS
            || limits.shutdown_grace.is_zero()
        {
            return Err(ServerError::InvalidLimits);
        }
        Ok(Self {
            broker,
            identities: Arc::new(identities),
            status,
            limits,
        })
    }

    pub async fn serve<F>(self, listener: UnixListener, shutdown: F) -> Result<(), ServerError>
    where
        F: Future<Output = ()> + Send,
    {
        let semaphore = Arc::new(Semaphore::new(self.limits.max_connections));
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);

        loop {
            while let Some(result) = tasks.try_join_next() {
                observe_task(result)?;
            }
            tokio::select! {
                () = &mut shutdown => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted.map_err(|source| ServerError::Accept { source })?;
                    let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                        drop(stream);
                        tracing::warn!(event = "broker_connection_rejected", reason = "connection_limit");
                        continue;
                    };
                    let broker = Arc::clone(&self.broker);
                    let identities = Arc::clone(&self.identities);
                    let status = self.status.clone();
                    let frame = self.limits.frame;
                    tasks.spawn(async move {
                        let _permit = permit;
                        handle(stream, &broker, &identities, &status, frame).await
                    });
                }
            }
        }
        drop(listener);

        let drain = async {
            let mut task_failed = false;
            while let Some(result) = tasks.join_next().await {
                if observe_task(result).is_err() {
                    task_failed = true;
                }
            }
            if task_failed {
                Err(ServerError::ConnectionTask)
            } else {
                Ok(())
            }
        };
        match timeout(self.limits.shutdown_grace, drain).await {
            Ok(result) => result,
            Err(_) => {
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
                Err(ServerError::ShutdownTimeout)
            }
        }
    }
}

fn observe_task(
    result: Result<Result<(), ConnectionError>, tokio::task::JoinError>,
) -> Result<(), ServerError> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            // The category answers "which failure class"; the chain answers "why", which is the
            // half that used to be dropped. `ENOSPC` during an audit append reaches an operator
            // only through this line.
            let cause = crate::error_chain(&error);
            // An unaudited outcome is the only connection failure an operator must act on: it
            // names the one invocation whose effect may have happened with nothing recording it.
            match error.unaudited_outcome() {
                Some(invocation) => tracing::error!(
                    event = "broker_outcome_unaudited",
                    category = error.category(),
                    invocation.id = %invocation,
                    error = %cause,
                ),
                None => tracing::warn!(
                    event = "broker_connection_failed",
                    category = error.category(),
                    error = %cause,
                ),
            }
            Ok(())
        }
        Err(_) => {
            tracing::error!(
                event = "broker_connection_failed",
                category = "task-failure"
            );
            Err(ServerError::ConnectionTask)
        }
    }
}

/// Stable, low-cardinality name for a framing failure.
///
/// The wire answer for every one of these is the same generic code, so this label is the only
/// thing that tells a slow client from an oversized frame from unreadable JSON.
const fn protocol_error_kind(error: &ProtocolError) -> &'static str {
    match error {
        ProtocolError::InvalidFrameLimit { .. } => "invalid-frame-limit",
        ProtocolError::ZeroTimeout => "zero-timeout",
        ProtocolError::Timeout => "timeout",
        ProtocolError::Io { .. } => "io",
        ProtocolError::EmptyFrame => "empty-frame",
        ProtocolError::FrameTooLarge { .. } => "frame-too-large",
        ProtocolError::Serialize { .. } => "serialize",
        ProtocolError::Deserialize { .. } => "deserialize",
    }
}

/// Records why a provider could not rewrite a command word.
///
/// The model is told only that the word could not be rewritten, which is right — a guest trap is
/// not something it can act on — so the host error has to land here or nowhere.
fn report_command_resolve_failure(word: &str, error: &dekopon_broker_host::BrokerHostError) {
    tracing::warn!(
        target: "dekopon_brokerd::audit",
        {
            audit.event = "command.resolve.failed",
            command.word = %word,
            error.kind = "provider",
            error = %crate::error_chain(error),
        },
        "command-word rewrite failed"
    );
}

async fn handle<A>(
    mut stream: UnixStream,
    broker: &Broker<A>,
    identities: &BTreeMap<u32, MappedPeer>,
    status: &ServiceStatus,
    limits: FrameLimits,
) -> Result<(), ConnectionError>
where
    A: AuditLog,
{
    let credentials = stream
        .peer_cred()
        .map_err(|source| ConnectionError::PeerCredentials { source })?;
    let uid = credentials.uid();
    let Some(peer) = identities.get(&uid) else {
        write_frame(
            &mut stream,
            &ResponseEnvelope::error(ERROR_UNAUTHENTICATED, "peer is not mapped by broker policy"),
            limits,
        )
        .await
        .map_err(ConnectionError::Write)?;
        return Ok(());
    };
    let request = match read_frame::<_, RequestEnvelope>(&mut stream, limits).await {
        Ok(request) => request,
        Err(error) => {
            // A timeout, an oversized frame, and unreadable JSON are one wire code and three
            // different operator problems. The kind and the bounded message stay here; the frame's
            // own contents never do, so a decode failure cannot become a payload channel.
            tracing::warn!(
                event = "broker_request_frame_invalid",
                error.kind = protocol_error_kind(&error),
                error = %error,
            );
            write_frame(
                &mut stream,
                &ResponseEnvelope::error(ERROR_INVALID_REQUEST, "request frame is invalid"),
                limits,
            )
            .await
            .map_err(ConnectionError::Write)?;
            return Err(ConnectionError::InvalidRequest);
        }
    };
    let context = &peer.context;
    let response = match request.request {
        BrokerRequest::Capabilities => ResponseEnvelope::capabilities(
            broker.capabilities(context),
            broker.command_words(context),
        ),
        BrokerRequest::CapabilitiesFor { subject, agent } => {
            match broker.capabilities_for(context, peer.attestor.as_ref(), &subject, &agent) {
                Some((capabilities, command_words)) => {
                    ResponseEnvelope::capabilities(capabilities, command_words)
                }
                // A refused attestation discloses nothing about what the attested context could
                // have seen — not even whether the subject is mapped.
                None => ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "attestation refused: no attestor authority for this subject",
                ),
            }
        }
        BrokerRequest::CapabilitiesForChat { claim } => {
            if !claim.scope.is_bounded() {
                ResponseEnvelope::error(ERROR_INVALID_REQUEST, "chat scope is invalid")
            } else {
                match broker.capabilities_for_chat(context, peer.attestor.as_ref(), &claim) {
                    Some((capabilities, command_words, memory)) => {
                        ResponseEnvelope::chat_capabilities(capabilities, command_words, memory)
                    }
                    None => ResponseEnvelope::error(
                        ERROR_UNAUTHENTICATED,
                        "chat attestation was refused",
                    ),
                }
            }
        }
        BrokerRequest::ResolveCommandForChat { claim, word, argv } => {
            if !claim.scope.is_bounded() {
                ResponseEnvelope::error(ERROR_INVALID_REQUEST, "chat scope is invalid")
            } else {
                match broker
                    .resolve_command_for_chat(context, peer.attestor.as_ref(), &claim, &word, &argv)
                    .await
                {
                    Ok(CommandResolution::Resolved { capability, input }) => {
                        ResponseEnvelope::command_resolution(capability, input)
                    }
                    Ok(CommandResolution::Failed { error }) => {
                        ResponseEnvelope::command_declined(error.message)
                    }
                    Err(error) => {
                        report_command_resolve_failure(&word, &error);
                        ResponseEnvelope::error(
                            ERROR_PROVIDER,
                            "command word could not be rewritten",
                        )
                    }
                }
            }
        }
        BrokerRequest::ResolveCommand { word, argv } => {
            match broker.resolve_command(&word, &argv).await {
                Ok(CommandResolution::Resolved { capability, input }) => {
                    ResponseEnvelope::command_resolution(capability, input)
                }
                // The provider declined this argv. That is a usage error for the model to read,
                // not a broker failure, so its own message travels back.
                Ok(CommandResolution::Failed { error }) => {
                    ResponseEnvelope::command_declined(error.message)
                }
                Err(error) => {
                    report_command_resolve_failure(&word, &error);
                    ResponseEnvelope::error(ERROR_PROVIDER, "command word could not be rewritten")
                }
            }
        }
        BrokerRequest::PublishAgentInventory { inventory } => {
            if peer.attestor.is_none() {
                ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "informational reports require a mapped gateway attestor",
                )
            } else if !inventory.is_valid() {
                ResponseEnvelope::error(ERROR_INVALID_REQUEST, "agent inventory is invalid")
            } else {
                let count = inventory.agents.len();
                status.replace_agents(inventory);
                tracing::debug!(
                    event = "broker_agent_inventory_updated",
                    agent.count = count
                );
                ResponseEnvelope::acknowledged()
            }
        }
        BrokerRequest::PublishModelUsage { usage } => {
            if peer.attestor.is_none() {
                ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "informational reports require a mapped gateway attestor",
                )
            } else if !usage.is_valid() {
                ResponseEnvelope::error(ERROR_INVALID_REQUEST, "model usage report is invalid")
            } else {
                status.record_usage(usage);
                tracing::debug!(
                    event = "broker_model_usage_updated",
                    model.call.count = usage.model_calls
                );
                ResponseEnvelope::acknowledged()
            }
        }
        BrokerRequest::InvokeForChat {
            invocation,
            attestation,
        } => {
            if attestation.invocation != invocation.id || !attestation.scope.is_bounded() {
                ResponseEnvelope::error(ERROR_INVALID_REQUEST, "chat attestation is invalid")
            } else {
                let storage = broker.capability_uses_storage(&invocation.capability);
                let span = if storage {
                    storage_invocation_span(&invocation.id, &invocation.trace)
                } else {
                    tracing::info_span!(
                        "broker.invocation", invocation = %invocation.id,
                        capability = %invocation.capability, trace = %invocation.trace,
                        subject = %attestation.subject, agent = %attestation.agent,
                    )
                };
                if let Some(parent) = invocation.trace_parent
                    && let Err(error) =
                        span.set_parent(dekopon_telemetry::remote_context(TraceContextParts {
                            trace_id: parent.trace_id(),
                            span_id: parent.parent_id(),
                            flags: parent.flags(),
                        }))
                {
                    tracing::debug!(event = "broker_trace_parent_ignored", error = %error);
                }
                match broker
                    .invoke_for_chat(context, peer.attestor.as_ref(), &attestation, invocation)
                    .instrument(span)
                    .await
                {
                    Ok(result) => ResponseEnvelope::invocation(result),
                    Err(error) => return write_broker_failure(&mut stream, limits, error).await,
                }
            }
        }
        BrokerRequest::RecordDeliveredTurnForChat { turn, attestation } => {
            if attestation.invocation != turn.id
                || !attestation.scope.is_bounded()
                || !turn.is_bounded()
                || !turn.delivery.is_canonical_for(&attestation.scope)
            {
                ResponseEnvelope::error(
                    ERROR_INVALID_REQUEST,
                    "delivered turn attestation is invalid",
                )
            } else {
                let span = storage_invocation_span(&turn.id, &turn.trace);
                if let Some(parent) = turn.trace_parent
                    && let Err(error) =
                        span.set_parent(dekopon_telemetry::remote_context(TraceContextParts {
                            trace_id: parent.trace_id(),
                            span_id: parent.parent_id(),
                            flags: parent.flags(),
                        }))
                {
                    tracing::debug!(event = "broker_trace_parent_ignored", error = %error);
                }
                match broker
                    .record_delivered_turn_for_chat(
                        context,
                        peer.attestor.as_ref(),
                        &attestation,
                        turn,
                    )
                    .instrument(span)
                    .await
                {
                    Ok(result) => ResponseEnvelope::invocation(result),
                    Err(error) => return write_broker_failure(&mut stream, limits, error).await,
                }
            }
        }
        BrokerRequest::InvokeFor {
            invocation,
            attestation,
        } => {
            // Structural binding is already one frame; this check is defense in depth and makes
            // a mismatched claim a protocol error rather than a policy decision.
            if attestation.invocation != invocation.id {
                write_frame(
                    &mut stream,
                    &ResponseEnvelope::error(
                        ERROR_INVALID_REQUEST,
                        "attestation is not bound to its proposal",
                    ),
                    limits,
                )
                .await
                .map_err(ConnectionError::Write)?;
                return Err(ConnectionError::InvalidRequest);
            }
            let span = if broker.capability_uses_storage(&invocation.capability) {
                storage_invocation_span(&invocation.id, &invocation.trace)
            } else {
                tracing::info_span!(
                    "broker.invocation",
                    invocation = %invocation.id,
                    capability = %invocation.capability,
                    trace = %invocation.trace,
                    subject = %attestation.subject,
                    agent = %attestation.agent,
                )
            };
            if let Some(parent) = invocation.trace_parent
                && let Err(error) =
                    span.set_parent(dekopon_telemetry::remote_context(TraceContextParts {
                        trace_id: parent.trace_id(),
                        span_id: parent.parent_id(),
                        flags: parent.flags(),
                    }))
            {
                tracing::debug!(event = "broker_trace_parent_ignored", error = %error);
            }
            match broker
                .invoke_for(context, peer.attestor.as_ref(), &attestation, invocation)
                .instrument(span)
                .await
            {
                Ok(result) => ResponseEnvelope::invocation(result),
                Err(error) => {
                    return write_broker_failure(&mut stream, limits, error).await;
                }
            }
        }
        BrokerRequest::Invoke { invocation } => {
            // Correlation identifiers only. Input, output, and every provider-facing value stay
            // out of this span for the same reason they stay out of audit records: telemetry is a
            // second egress path and must not carry what the audit chain deliberately redacts.
            let span = if broker.capability_uses_storage(&invocation.capability) {
                storage_invocation_span(&invocation.id, &invocation.trace)
            } else {
                tracing::info_span!(
                    "broker.invocation",
                    invocation = %invocation.id,
                    capability = %invocation.capability,
                    trace = %invocation.trace,
                )
            };
            // An untrusted client chooses this parent. It reaches telemetry correlation and
            // nothing else: policy, replay rejection, and audit never read it.
            if let Some(parent) = invocation.trace_parent
                && let Err(error) =
                    span.set_parent(dekopon_telemetry::remote_context(TraceContextParts {
                        trace_id: parent.trace_id(),
                        span_id: parent.parent_id(),
                        flags: parent.flags(),
                    }))
            {
                // Losing correlation is not losing the invocation: the span still records, just as
                // its own root, and the durable audit is unaffected either way.
                tracing::debug!(event = "broker_trace_parent_ignored", error = %error);
            }
            match broker.invoke(context, invocation).instrument(span).await {
                Ok(result) => ResponseEnvelope::invocation(result),
                Err(error) => {
                    return write_broker_failure(&mut stream, limits, error).await;
                }
            }
        }
    };
    write_frame(&mut stream, &response, limits)
        .await
        .map_err(ConnectionError::Write)
}

/// Reports a broker failure while preserving whether provider work may already have completed.
///
/// Collapsing the two cases into one code would invite a resubmission that duplicates a
/// non-idempotent external effect, so the distinction the broker library draws survives the
/// wire boundary.
async fn write_broker_failure(
    stream: &mut UnixStream,
    limits: FrameLimits,
    error: BrokerError,
) -> Result<(), ConnectionError> {
    let (code, message, failure) = if let Some(invocation) = error.unaudited_outcome() {
        let invocation = invocation.clone();
        (
            ERROR_OUTCOME_UNAUDITED,
            "provider work may already have completed and its outcome was not audited",
            ConnectionError::OutcomeUnaudited {
                invocation,
                source: error,
            },
        )
    } else if let Some(code) = error.storage_failure_code() {
        (
            code,
            "broker-owned provider storage failed before provider execution",
            ConnectionError::Broker { source: error },
        )
    } else {
        (
            ERROR_BROKER_UNAVAILABLE,
            "broker could not durably complete the request",
            ConnectionError::Broker { source: error },
        )
    };
    write_frame(stream, &ResponseEnvelope::error(code, message), limits)
        .await
        .map_err(ConnectionError::Write)?;
    Err(failure)
}

#[derive(Debug, Error)]
enum ConnectionError {
    #[error("peer credentials unavailable")]
    PeerCredentials {
        #[source]
        source: io::Error,
    },
    #[error("invalid request")]
    InvalidRequest,
    #[error("broker could not audit the outcome of {invocation}")]
    OutcomeUnaudited {
        /// Invocation whose external effect may already have completed.
        invocation: InvocationId,
        /// The broker failure that ended the request, kept so the log can name its cause.
        #[source]
        source: BrokerError,
    },
    #[error("broker failed")]
    Broker {
        /// The broker failure the wire code deliberately generalizes.
        #[source]
        source: BrokerError,
    },
    #[error("response write failed")]
    Write(#[source] ProtocolError),
}

impl ConnectionError {
    /// Invocation whose provider work may already have completed with no terminal audit record.
    const fn unaudited_outcome(&self) -> Option<&InvocationId> {
        match self {
            Self::OutcomeUnaudited { invocation, .. } => Some(invocation),
            Self::PeerCredentials { .. }
            | Self::InvalidRequest
            | Self::Broker { .. }
            | Self::Write(_) => None,
        }
    }

    const fn category(&self) -> &'static str {
        match self {
            Self::PeerCredentials { .. } => "peer-credentials",
            Self::InvalidRequest => "invalid-request",
            Self::OutcomeUnaudited { .. } => "broker-outcome-unaudited",
            Self::Broker { .. } => "broker",
            Self::Write(_) => "response-write",
        }
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("server limits must be positive and valid")]
    InvalidLimits,
    #[error("could not accept a broker connection")]
    Accept {
        #[source]
        source: io::Error,
    },
    #[error("a broker connection task failed internally")]
    ConnectionTask,
    #[error("broker connections did not finish within the shutdown grace period")]
    ShutdownTimeout,
}
