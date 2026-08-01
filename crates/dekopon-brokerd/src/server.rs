use std::{collections::BTreeMap, future::Future, io, sync::Arc, time::Duration};

use dekopon_broker::{AuditLog, AuthenticatedContext, Broker, BrokerError};
use dekopon_broker_protocol::{
    BrokerRequest, ERROR_BROKER_UNAVAILABLE, ERROR_INVALID_REQUEST, ERROR_OUTCOME_UNAUDITED,
    ERROR_UNAUTHENTICATED, FrameLimits, ProtocolError, RequestEnvelope, ResponseEnvelope,
    read_frame, write_frame,
};
use dekopon_core::InvocationId;
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
    time::timeout,
};

use crate::config::HARD_MAX_CONNECTIONS;

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
    identities: Arc<BTreeMap<u32, AuthenticatedContext>>,
    limits: ServerLimits,
}

impl<A> BrokerServer<A>
where
    A: AuditLog + 'static,
{
    pub fn new(
        broker: Arc<Broker<A>>,
        identities: BTreeMap<u32, AuthenticatedContext>,
        limits: ServerLimits,
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
            // An unaudited outcome is the only connection failure an operator must act on: it
            // names the one invocation whose effect may have happened with nothing recording it.
            match error.unaudited_outcome() {
                Some(invocation) => tracing::error!(
                    event = "broker_outcome_unaudited",
                    category = error.category(),
                    invocation.id = %invocation
                ),
                None => tracing::warn!(
                    event = "broker_connection_failed",
                    category = error.category()
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

async fn handle<A>(
    mut stream: UnixStream,
    broker: &Broker<A>,
    identities: &BTreeMap<u32, AuthenticatedContext>,
    limits: FrameLimits,
) -> Result<(), ConnectionError>
where
    A: AuditLog,
{
    let credentials = stream
        .peer_cred()
        .map_err(|source| ConnectionError::PeerCredentials { source })?;
    let uid = credentials.uid();
    let Some(context) = identities.get(&uid) else {
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
        Err(_) => {
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
    let response = match request.request {
        BrokerRequest::Capabilities => ResponseEnvelope::capabilities(broker.capabilities(context)),
        BrokerRequest::Invoke { invocation } => match broker.invoke(context, invocation).await {
            Ok(result) => ResponseEnvelope::invocation(result),
            Err(error) => {
                return write_broker_failure(&mut stream, limits, &error).await;
            }
        },
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
    error: &BrokerError,
) -> Result<(), ConnectionError> {
    let (code, message, failure) = match error.unaudited_outcome() {
        Some(invocation) => (
            ERROR_OUTCOME_UNAUDITED,
            "provider work may already have completed and its outcome was not audited",
            ConnectionError::OutcomeUnaudited {
                invocation: invocation.clone(),
            },
        ),
        None => (
            ERROR_BROKER_UNAVAILABLE,
            "broker could not durably complete the request",
            ConnectionError::Broker,
        ),
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
    },
    #[error("broker failed")]
    Broker,
    #[error("response write failed")]
    Write(#[source] ProtocolError),
}

impl ConnectionError {
    /// Invocation whose provider work may already have completed with no terminal audit record.
    const fn unaudited_outcome(&self) -> Option<&InvocationId> {
        match self {
            Self::OutcomeUnaudited { invocation } => Some(invocation),
            Self::PeerCredentials { .. } | Self::InvalidRequest | Self::Broker | Self::Write(_) => {
                None
            }
        }
    }

    const fn category(&self) -> &'static str {
        match self {
            Self::PeerCredentials { .. } => "peer-credentials",
            Self::InvalidRequest => "invalid-request",
            Self::OutcomeUnaudited { .. } => "broker-outcome-unaudited",
            Self::Broker => "broker",
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
