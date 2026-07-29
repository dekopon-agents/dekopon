use std::{collections::BTreeMap, future::Future, io, sync::Arc, time::Duration};

use dekopon_broker::{AuditLog, AuthenticatedContext, Broker};
use dekopon_broker_protocol::{
    BrokerRequest, FrameLimits, ProtocolError, RequestEnvelope, ResponseEnvelope, read_frame,
    write_frame,
};
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
            tracing::warn!(
                event = "broker_connection_failed",
                category = error.category()
            );
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
            &ResponseEnvelope::error("unauthenticated", "peer is not mapped by broker policy"),
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
                &ResponseEnvelope::error("invalid-request", "request frame is invalid"),
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
            Err(_) => {
                return write_broker_failure(&mut stream, limits).await;
            }
        },
    };
    write_frame(&mut stream, &response, limits)
        .await
        .map_err(ConnectionError::Write)
}

async fn write_broker_failure(
    stream: &mut UnixStream,
    limits: FrameLimits,
) -> Result<(), ConnectionError> {
    write_frame(
        stream,
        &ResponseEnvelope::error(
            "broker-unavailable",
            "broker could not durably complete the request",
        ),
        limits,
    )
    .await
    .map_err(ConnectionError::Write)?;
    Err(ConnectionError::Broker)
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
    #[error("broker failed")]
    Broker,
    #[error("response write failed")]
    Write(#[source] ProtocolError),
}

impl ConnectionError {
    const fn category(&self) -> &'static str {
        match self {
            Self::PeerCredentials { .. } => "peer-credentials",
            Self::InvalidRequest => "invalid-request",
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
