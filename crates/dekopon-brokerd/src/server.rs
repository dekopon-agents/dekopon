use std::{collections::BTreeMap, future::Future, io, sync::Arc, time::Duration};

use dekopon_broker::{AttestorGrant, AuditLog, AuthenticatedContext, Broker, BrokerError};
use dekopon_broker_protocol::{
    Attestation, BrokerRequest, CommandRunOutcome, DescriptorStream, ERROR_BROKER_UNAVAILABLE,
    ERROR_CAPACITY_EXHAUSTED, ERROR_INVALID_REQUEST, ERROR_OUTCOME_UNAUDITED, ERROR_PROVIDER,
    ERROR_UNAUTHENTICATED, FrameLimits, InvocationRequest, ProtocolError, RequestEnvelope,
    ResponseEnvelope, TraceParent,
};
use dekopon_core::{
    ACCEPT_BACKOFF_MS, InvocationId, MAX_ACCEPT_BACKOFF_MS, retryable_accept_error,
};
use dekopon_telemetry::TraceContextParts;
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

#[derive(Clone, Debug)]
pub struct MappedPeer {
    pub context: AuthenticatedContext,
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
        limits
            .frame
            .validate()
            .map_err(|source| ServerError::InvalidFrameLimits { source })?;
        if limits.max_connections == 0
            || limits.max_connections > HARD_MAX_CONNECTIONS
            || limits.shutdown_grace.is_zero()
        {
            return Err(ServerError::InvalidLimits);
        }
        Ok(Self {
            broker,
            identities: Arc::new(identities),
            limits,
        })
    }

    pub async fn serve<F>(self, listener: UnixListener, shutdown: F) -> Result<(), ServerError>
    where
        F: Future<Output = ()> + Send,
    {
        let semaphore = Arc::new(Semaphore::new(self.limits.max_connections));
        let mut tasks = JoinSet::new();
        let mut accept_backoff_ms = ACCEPT_BACKOFF_MS;
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                // Without this branch, a finished connection's outcome is only logged once the next
                // connection arrives, delaying the failure an operator must act on.
                Some(result) = tasks.join_next(), if !tasks.is_empty() => observe_task(result)?,
                accepted = listener.accept() => {
                    let stream = match accepted {
                        Ok((stream, _)) => {
                            accept_backoff_ms = ACCEPT_BACKOFF_MS;
                            stream
                        }
                        Err(source) => {
                            let Some(kind) = retryable_accept_error(&source) else {
                                return Err(ServerError::Accept { source });
                            };
                            tracing::warn!(
                                event = "broker_accept_retried",
                                error.kind = kind,
                                backoff_ms = accept_backoff_ms,
                                error = %dekopon_core::error_chain(&source),
                            );
                            tokio::time::sleep(Duration::from_millis(accept_backoff_ms)).await;
                            accept_backoff_ms =
                                accept_backoff_ms.saturating_mul(2).min(MAX_ACCEPT_BACKOFF_MS);
                            continue;
                        }
                    };
                    let Ok(permit) = Arc::clone(&semaphore).try_acquire_owned() else {
                        drop(stream);
                        tracing::warn!(event = "broker_connection_rejected", reason = "connection_limit");
                        continue;
                    };
                    let broker = Arc::clone(&self.broker);
                    let identities = Arc::clone(&self.identities);
                    let frame = self.limits.frame;
                    tasks.spawn(async move {
                        let _permit = permit;
                        handle(stream, &broker, &identities, frame).await
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
            let cause = dekopon_core::error_chain(&error);
            match error.unaudited_outcome() {
                Some(invocation) => tracing::error!(
                    event = "broker_outcome_unaudited",
                    category = error.category(),
                    invocation.id = %invocation,
                    error = %cause,
                ),
                None if error.is_capacity_exhausted() => tracing::error!(
                    event = "broker_capacity_exhausted",
                    category = error.category(),
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

/// Every framing failure gets the same generic wire code; this label is the only way to tell a slow
/// client from an oversized frame from bad JSON.
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
        ProtocolError::DescriptorsTruncated => "descriptors-truncated",
        ProtocolError::TooManyDescriptors => "too-many-descriptors",
        ProtocolError::UnexpectedDescriptors => "unexpected-descriptors",
        ProtocolError::DescriptorIndex => "descriptor-index",
        ProtocolError::DescriptorFlags { .. } => "descriptor-flags",
        ProtocolError::TooManyAssetRows => "too-many-asset-rows",
    }
}

fn report_command_run_failure(word: &str, error: &dekopon_broker_host::BrokerHostError) {
    tracing::warn!(
        target: "dekopon_brokerd::audit",
        {
            audit.event = "command.resolve.failed",
            command.word = %word,
            error.kind = "provider",
            error = %dekopon_core::error_chain(error),
        },
        "command-word run failed"
    );
}

/// This checks only structural validity, not authorization: a well-formed claim can still be
/// refused later, and failing here authorizes, accounts, or audits nothing.
fn claim_is_valid(attestation: Option<&Attestation>, proposal: Option<&InvocationId>) -> bool {
    attestation.is_none_or(|claim| {
        claim.is_well_formed()
            && proposal.map_or_else(|| claim.invocation.is_none(), |id| claim.binds(id))
    })
}

async fn refuse_invalid_claim(
    stream: &mut DescriptorStream,
    limits: FrameLimits,
) -> Result<(), ConnectionError> {
    stream
        .write_frame(
            &ResponseEnvelope::error(ERROR_INVALID_REQUEST, "attestation is invalid"),
            &[],
            limits,
        )
        .await
        .map_err(ConnectionError::Write)?;
    Err(ConnectionError::InvalidRequest)
}

fn invocation_span(
    request: &InvocationRequest,
    attestation: Option<&Attestation>,
) -> tracing::Span {
    match attestation {
        Some(claim) => tracing::info_span!(
            "broker.invocation",
            invocation = %request.id,
            capability = %request.capability,
            trace = %request.trace_parent.trace(),
            subject = %claim.subject,
            agent = %claim.agent,
        ),
        None => tracing::info_span!(
            "broker.invocation",
            invocation = %request.id,
            capability = %request.capability,
            trace = %request.trace_parent.trace(),
        ),
    }
}

const fn command_run_outcome(outcome: &CommandRunOutcome) -> &'static str {
    match outcome {
        CommandRunOutcome::Proposed { .. } => "proposed",
        CommandRunOutcome::Rendered { .. } => "rendered",
        CommandRunOutcome::Failed { .. } => "failed",
    }
}

/// The client picks this trace parent, so it's used only for correlation, never policy; a parent
/// the SDK rejects just becomes its own root span.
fn adopt_trace_parent(span: &tracing::Span, parent: TraceParent) {
    if let Err(error) = span.set_parent(dekopon_telemetry::remote_context(TraceContextParts {
        trace_id: parent.trace_id(),
        span_id: parent.parent_id(),
        flags: parent.flags(),
    })) {
        tracing::debug!(event = "broker_trace_parent_ignored", error = %error);
    }
}

async fn handle<A>(
    stream: UnixStream,
    broker: &Broker<A>,
    identities: &BTreeMap<u32, MappedPeer>,
    limits: FrameLimits,
) -> Result<(), ConnectionError>
where
    A: AuditLog,
{
    let credentials = stream
        .peer_cred()
        .map_err(|source| ConnectionError::PeerCredentials { source })?;
    let uid = credentials.uid();
    let mut stream = DescriptorStream::new(stream);
    let Some(peer) = identities.get(&uid) else {
        // The wire refusal is deliberately opaque; an unmapped UID is also the usual reason a
        // broker's readiness probe fails, since it connects as the broker's own UID.
        tracing::warn!(event = "broker_peer_unmapped", peer.uid = uid);
        stream
            .write_frame(
                &ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "peer is not mapped by broker policy",
                ),
                &[],
                limits,
            )
            .await
            .map_err(ConnectionError::Write)?;
        return Ok(());
    };
    let received =
        stream
            .read_frame::<RequestEnvelope>(limits)
            .await
            .and_then(|(request, descriptors)| {
                request.request.validate()?;
                if !descriptors.is_empty()
                    && !matches!(request.request, BrokerRequest::Invoke { .. })
                {
                    return Err(ProtocolError::UnexpectedDescriptors);
                }
                Ok((request, descriptors))
            });
    let (request, descriptors) = match received {
        Ok(request) => request,
        Err(error) => {
            // Timeout, oversized frame, and bad JSON share one wire code; only the bounded message
            // is logged, never the frame's bytes, so decoding can't leak data.
            tracing::warn!(
                event = "broker_request_frame_invalid",
                error.kind = protocol_error_kind(&error),
                error = %error,
            );
            stream
                .write_frame(
                    &ResponseEnvelope::error(ERROR_INVALID_REQUEST, "request frame is invalid"),
                    &[],
                    limits,
                )
                .await
                .map_err(ConnectionError::Write)?;
            return Err(ConnectionError::InvalidRequest);
        }
    };
    let context = &peer.context;
    let mut outputs = dekopon_broker_host::asset::AssetOutputs::default();
    let response = match request.request {
        BrokerRequest::Capabilities { attestation } => {
            if !claim_is_valid(attestation.as_ref(), None) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            match broker.capability_surface(context, peer.attestor.as_ref(), attestation.as_ref()) {
                Some((capabilities, command_words, chat_memory)) => {
                    ResponseEnvelope::chat_capabilities(capabilities, command_words, chat_memory)
                }
                // A refused attestation reveals nothing about the attested context, not even
                // whether the subject is mapped.
                None => ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "attestation refused: no attestor authority for this subject",
                ),
            }
        }
        BrokerRequest::RunCommand {
            attestation,
            trace_parent,
            word,
            argv,
            stdin,
        } => {
            if !claim_is_valid(attestation.as_ref(), None) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            let span = tracing::info_span!(
                "broker.command_run",
                word = %word,
                outcome = tracing::field::Empty,
            );
            adopt_trace_parent(&span, trace_parent);
            match broker
                .run_command(
                    context,
                    peer.attestor.as_ref(),
                    attestation.as_ref(),
                    &word,
                    &argv,
                    stdin.as_deref(),
                )
                .instrument(span.clone())
                .await
            {
                Ok(result) => {
                    span.record("outcome", command_run_outcome(&result));
                    ResponseEnvelope::command_run(result)
                }
                Err(error) => {
                    span.record("outcome", "error");
                    span.in_scope(|| report_command_run_failure(&word, &error));
                    ResponseEnvelope::error(ERROR_PROVIDER, "command word could not be run")
                }
            }
        }
        BrokerRequest::Invoke {
            attestation,
            invocation,
            assets,
            sends_remaining,
        } => {
            if !claim_is_valid(attestation.as_ref(), Some(&invocation.id)) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            let span = invocation_span(&invocation, attestation.as_ref());
            adopt_trace_parent(&span, invocation.trace_parent);
            match broker
                .invoke(
                    context,
                    peer.attestor.as_ref(),
                    attestation.as_ref(),
                    invocation,
                    dekopon_broker_host::asset::AssetInputs {
                        rows: assets,
                        descriptors,
                        sends_remaining,
                    },
                )
                .instrument(span)
                .await
            {
                Ok(outcome) => {
                    outputs = outcome.assets;
                    ResponseEnvelope::invocation(
                        outcome.result,
                        std::mem::take(&mut outputs.attached),
                        std::mem::take(&mut outputs.removed),
                        std::mem::take(&mut outputs.sent),
                    )
                }
                Err(error) => return write_broker_failure(&mut stream, limits, error).await,
            }
        }
        BrokerRequest::RecordDeliveredTurn { attestation, turn } => {
            if !claim_is_valid(Some(&attestation), Some(&turn.id))
                || !turn.is_bounded()
                || !attestation
                    .scope
                    .as_ref()
                    .is_some_and(|scope| turn.delivery.is_canonical_for(scope))
            {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            let span = tracing::info_span!(
                "broker.invocation",
                invocation = %turn.id,
                trace = %turn.trace_parent.trace(),
                subject = %attestation.subject,
                agent = %attestation.agent,
            );
            adopt_trace_parent(&span, turn.trace_parent);
            match broker
                .record_delivered_turn(context, peer.attestor.as_ref(), &attestation, turn)
                .instrument(span)
                .await
            {
                Ok(result) => ResponseEnvelope::invocation(result, vec![], vec![], vec![]),
                Err(error) => return write_broker_failure(&mut stream, limits, error).await,
            }
        }
    };
    use std::os::fd::AsFd as _;
    let descriptors = outputs
        .files
        .iter()
        .map(|file| file.file().as_fd())
        .collect::<Vec<_>>();
    stream
        .write_frame(&response, &descriptors, limits)
        .await
        .map_err(ConnectionError::Write)
}

/// Collapsing this into one wire code would invite retries that duplicate a non-idempotent external
/// effect, so the completed-or-not distinction crosses the wire intact.
async fn write_broker_failure(
    stream: &mut DescriptorStream,
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
    } else if error.capacity_failure_code().is_some() {
        (
            ERROR_CAPACITY_EXHAUSTED,
            "a bounded broker resource is exhausted and will not recover without operator action",
            ConnectionError::CapacityExhausted { source: error },
        )
    } else if let Some(code) = error.storage_failure_code() {
        (
            code,
            if error.storage_namespace_reset() {
                "storage for this conversation was corrupt and has been reset; retry"
            } else {
                "broker-owned provider storage failed before provider execution"
            },
            ConnectionError::Broker { source: error },
        )
    } else {
        (
            ERROR_BROKER_UNAVAILABLE,
            "broker could not durably complete the request",
            ConnectionError::Broker { source: error },
        )
    };
    stream
        .write_frame(&ResponseEnvelope::error(code, message), &[], limits)
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
    #[error("a bounded broker resource is exhausted")]
    CapacityExhausted {
        #[source]
        source: BrokerError,
    },
    #[error("broker could not audit the outcome of {invocation}")]
    OutcomeUnaudited {
        invocation: InvocationId,
        #[source]
        source: BrokerError,
    },
    #[error("broker failed")]
    Broker {
        #[source]
        source: BrokerError,
    },
    #[error("response write failed")]
    Write(#[source] ProtocolError),
}

impl ConnectionError {
    const fn unaudited_outcome(&self) -> Option<&InvocationId> {
        match self {
            Self::OutcomeUnaudited { invocation, .. } => Some(invocation),
            Self::PeerCredentials { .. }
            | Self::InvalidRequest
            | Self::CapacityExhausted { .. }
            | Self::Broker { .. }
            | Self::Write(_) => None,
        }
    }

    const fn is_capacity_exhausted(&self) -> bool {
        matches!(self, Self::CapacityExhausted { .. })
    }

    const fn category(&self) -> &'static str {
        match self {
            Self::PeerCredentials { .. } => "peer-credentials",
            Self::InvalidRequest => "invalid-request",
            Self::OutcomeUnaudited { .. } => "broker-outcome-unaudited",
            Self::CapacityExhausted { .. } => "broker-capacity-exhausted",
            Self::Broker { .. } => "broker",
            Self::Write(_) => "response-write",
        }
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("server limits must be positive and valid")]
    InvalidLimits,
    #[error("broker frame limits are invalid")]
    InvalidFrameLimits {
        #[source]
        source: ProtocolError,
    },
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
