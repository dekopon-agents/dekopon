use std::{collections::BTreeMap, future::Future, io, sync::Arc, time::Duration};

use dekopon_broker::{AttestorGrant, AuditLog, AuthenticatedContext, Broker, BrokerError};
use dekopon_broker_host::CommandRunOutcome;
use dekopon_broker_protocol::{
    Attestation, BrokerRequest, ERROR_BROKER_UNAVAILABLE, ERROR_CAPACITY_EXHAUSTED,
    ERROR_INVALID_REQUEST, ERROR_OUTCOME_UNAUDITED, ERROR_PROVIDER, ERROR_UNAUTHENTICATED,
    FrameLimits, InvocationRequest, ProtocolError, RequestEnvelope, ResponseEnvelope, TraceParent,
    read_frame, write_frame,
};
use dekopon_core::{
    ACCEPT_BACKOFF_MS, InvocationId, MAX_ACCEPT_BACKOFF_MS, TraceId, retryable_accept_error,
};
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
/// operations, and only an attested operation consults the grant at all.
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
        let mut accept_backoff_ms = ACCEPT_BACKOFF_MS;
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                // Without this branch a finished connection is only observed when the *next* one
                // arrives, so `broker_outcome_unaudited` — the one failure an operator must act
                // on — waits on unrelated traffic to be logged at all.
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
            let cause = dekopon_core::error_chain(&error);
            // An unaudited outcome is the only connection failure an operator must act on: it
            // names the one invocation whose effect may have happened with nothing recording it.
            match error.unaudited_outcome() {
                Some(invocation) => tracing::error!(
                    event = "broker_outcome_unaudited",
                    category = error.category(),
                    invocation.id = %invocation,
                    error = %cause,
                ),
                // Exhaustion is the second one an operator must act on, and the only signal it
                // produces: every client sees a refusal it cannot retry out of, and nothing else
                // in the process reports that the bound was reached.
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

/// Records why a provider could not run a command word.
///
/// The model is told only that the word could not be run, which is right — a guest trap or an
/// input past the host bound is not something it can act on from the message — so the host error
/// has to land here or nowhere. The event keeps its `command.resolve.failed` name: it is the
/// operator-facing identifier for this class on both the run and the legacy rewrite operation.
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

/// Whether a claim is structurally usable, before any grant is consulted.
///
/// Attestation shape is one axis, so this is one check for every operation that can carry one.
/// `proposal` is the identifier the claim must bind to; `None` names an operation with no proposal,
/// where a bound claim is itself malformed because there is nothing for it to bind to. Neither
/// half is an authorization decision — the broker still refuses a well-formed claim it does not
/// honor — and a request that fails here is answered `invalid-request` with nothing authorized,
/// accounted, or audited.
fn claim_is_valid(attestation: Option<&Attestation>, proposal: Option<&InvocationId>) -> bool {
    attestation.is_none_or(|claim| {
        claim.is_well_formed()
            && proposal.map_or_else(|| claim.invocation.is_none(), |id| claim.binds(id))
    })
}

/// Answers a malformed claim with the stable protocol code and ends the connection.
///
/// The message stays generic on purpose: which half of the claim was malformed is a property of a
/// frame the peer built, and a client that cannot bind its own attestation cannot act on detail.
async fn refuse_invalid_claim(
    stream: &mut UnixStream,
    limits: FrameLimits,
) -> Result<(), ConnectionError> {
    write_frame(
        stream,
        &ResponseEnvelope::error(ERROR_INVALID_REQUEST, "attestation is invalid"),
        limits,
    )
    .await
    .map_err(ConnectionError::Write)?;
    Err(ConnectionError::InvalidRequest)
}

/// The invocation span, carrying the attested subject and agent only when a claim named them.
///
/// A storage-routed proposal drops the capability as well, because the chat-memory routes make the
/// capability identifier itself a statement about the sender.
fn invocation_span(
    request: &InvocationRequest,
    attestation: Option<&Attestation>,
    storage: bool,
) -> tracing::Span {
    if storage {
        return storage_invocation_span(&request.id, &request.trace);
    }
    match attestation {
        Some(claim) => tracing::info_span!(
            "broker.invocation",
            invocation = %request.id,
            capability = %request.capability,
            trace = %request.trace,
            subject = %claim.subject,
            agent = %claim.agent,
        ),
        None => tracing::info_span!(
            "broker.invocation",
            invocation = %request.id,
            capability = %request.capability,
            trace = %request.trace,
        ),
    }
}

/// Joins the span to the client's trace when it offered one.
///
/// An untrusted client chooses this parent. It reaches telemetry correlation and nothing else:
/// policy, replay rejection, and audit never read it. Losing the correlation is not losing the
/// invocation either — the span still records, just as its own root — so a rejected `traceparent`
/// is a debug event rather than a failure.
fn adopt_trace_parent(span: &tracing::Span, parent: Option<TraceParent>) {
    if let Some(parent) = parent
        && let Err(error) = span.set_parent(dekopon_telemetry::remote_context(TraceContextParts {
            trace_id: parent.trace_id(),
            span_id: parent.parent_id(),
            flags: parent.flags(),
        }))
    {
        tracing::debug!(event = "broker_trace_parent_ignored", error = %error);
    }
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
        BrokerRequest::AuthorizeControl {
            attestation,
            proposal,
        } => {
            if !proposal.is_well_formed(attestation.as_ref()) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            let span = tracing::info_span!("broker.control", control = %proposal.id,
                job = %proposal.scope.job, request = %proposal.scope.request,
                sequence = proposal.sequence);
            adopt_trace_parent(&span, proposal.trace_parent);
            match broker
                .authorize_control(
                    context,
                    peer.attestor.as_ref(),
                    attestation.as_ref(),
                    proposal,
                )
                .instrument(span)
                .await
            {
                Ok(decision) => ResponseEnvelope {
                    api_version: dekopon_broker_protocol::ProtocolVersion::V1Alpha3,
                    response: dekopon_broker_protocol::BrokerResponse::ControlDecision {
                        decision: Box::new(decision),
                    },
                },
                Err(error) => return write_broker_failure(&mut stream, limits, error).await,
            }
        }
        BrokerRequest::Capabilities { attestation } => {
            if !claim_is_valid(attestation.as_ref(), None) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            match broker.capability_surface(context, peer.attestor.as_ref(), attestation.as_ref()) {
                Some((capabilities, command_words, chat_memory)) => {
                    ResponseEnvelope::chat_capabilities(
                        capabilities,
                        command_words,
                        chat_memory,
                        broker.surface_epoch().clone(),
                    )
                }
                // A refused attestation discloses nothing about what the attested context could
                // have seen — not even whether the subject is mapped.
                None => ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "attestation refused: no attestor authority for this subject",
                ),
            }
        }
        BrokerRequest::ResolveCommand {
            attestation,
            word,
            argv,
        } => {
            if !claim_is_valid(attestation.as_ref(), None) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            // The legacy operation: the same run with no piped value, answered in the shape an
            // older client reads. Rendered text has nowhere to go on that shape, so it travels as
            // a decline carrying the text, stdout first and then stderr, exactly as it did before
            // the run operation existed.
            match broker
                .run_command(
                    context,
                    peer.attestor.as_ref(),
                    attestation.as_ref(),
                    &word,
                    &argv,
                    None,
                )
                .await
            {
                Ok(CommandRunOutcome::Proposed { capability, input }) => {
                    ResponseEnvelope::command_resolution(capability, input)
                }
                // The provider declined this argv. That is a usage error for the model to read,
                // not a broker failure, so its own message travels back.
                Ok(CommandRunOutcome::Failed { error }) => {
                    ResponseEnvelope::command_declined(error.message)
                }
                Ok(CommandRunOutcome::Rendered { stdout, stderr, .. }) => {
                    ResponseEnvelope::command_declined(format!("{stdout}{stderr}"))
                }
                Err(error) => {
                    report_command_run_failure(&word, &error);
                    ResponseEnvelope::error(ERROR_PROVIDER, "command word could not be run")
                }
            }
        }
        BrokerRequest::RunCommand {
            attestation,
            word,
            argv,
            stdin,
        } => {
            if !claim_is_valid(attestation.as_ref(), None) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            match broker
                .run_command(
                    context,
                    peer.attestor.as_ref(),
                    attestation.as_ref(),
                    &word,
                    &argv,
                    stdin.as_deref(),
                )
                .await
            {
                // Whatever the guest answered travels intact: a proposal the caller submits next,
                // text the guest rendered with the status it chose, or its own decline.
                Ok(result) => ResponseEnvelope::command_run(result),
                // Everything else — no such word, a trap, an input past the host bound, a host
                // import — is the one opaque answer, with the cause recorded on this side.
                Err(error) => {
                    report_command_run_failure(&word, &error);
                    ResponseEnvelope::error(ERROR_PROVIDER, "command word could not be run")
                }
            }
        }
        BrokerRequest::Invoke {
            attestation,
            invocation,
        } => {
            // Structural binding is already one frame; this check is defense in depth and makes a
            // mismatched or malformed claim a protocol error rather than a policy decision.
            if !claim_is_valid(attestation.as_ref(), Some(&invocation.id)) {
                return refuse_invalid_claim(&mut stream, limits).await;
            }
            // Correlation identifiers only. Input, output, and every provider-facing value stay
            // out of this span for the same reason they stay out of audit records: telemetry is a
            // second egress path and must not carry what the audit chain deliberately redacts.
            let span = invocation_span(
                &invocation,
                attestation.as_ref(),
                broker.capability_uses_storage(&invocation.capability),
            );
            adopt_trace_parent(&span, invocation.trace_parent);
            match broker
                .invoke(
                    context,
                    peer.attestor.as_ref(),
                    attestation.as_ref(),
                    invocation,
                )
                .instrument(span)
                .await
            {
                Ok(result) => ResponseEnvelope::invocation(result),
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
            let span = storage_invocation_span(&turn.id, &turn.trace);
            adopt_trace_parent(&span, turn.trace_parent);
            match broker
                .record_delivered_turn(context, peer.attestor.as_ref(), &attestation, turn)
                .instrument(span)
                .await
            {
                Ok(result) => ResponseEnvelope::invocation(result),
                Err(error) => return write_broker_failure(&mut stream, limits, error).await,
            }
        }
        BrokerRequest::PublishAgentInventory { inventory } => {
            if peer.attestor.is_none() {
                ResponseEnvelope::error(
                    ERROR_UNAUTHENTICATED,
                    "informational reports require a mapped gateway attestor",
                )
            } else if let Err(error) = inventory.validate() {
                // The wire message stays generic; the specific bound and agent are an operator
                // diagnostic, and `InventoryError` carries only identifiers and byte counts.
                tracing::warn!(event = "broker_agent_inventory_rejected", reason = %error);
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
            } else if let Err(error) = usage.validate() {
                tracing::warn!(event = "broker_model_usage_rejected", reason = %error);
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
    let (code, message, failure) = if matches!(error, BrokerError::InvalidControl) {
        (
            ERROR_INVALID_REQUEST,
            "control binding is invalid",
            ConnectionError::Broker { source: error },
        )
    } else if let Some(invocation) = error.unaudited_outcome() {
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
    #[error("a bounded broker resource is exhausted")]
    CapacityExhausted {
        /// The exhaustion the wire code names.
        #[source]
        source: BrokerError,
    },
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
            | Self::CapacityExhausted { .. }
            | Self::Broker { .. }
            | Self::Write(_) => None,
        }
    }

    /// Whether the failure ends every subsequent request the same way until an operator acts.
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
        /// Which frame bound was rejected: a zero or over-ceiling maximum, or a zero I/O timeout.
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
