use std::{future::poll_fn, io, sync::Arc};

use dekopon_process::{LimitExceeded, ShellThreads, ThreadName, ThreadPermit};
use parking_lot::Mutex;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinSet,
};
use tracing::Instrument as _;

use crate::session::TaskFailure;

#[derive(Clone, Copy)]
pub(crate) enum Work {
    Turn,
    Job,
    Tick,
    Memory,
}

impl Work {
    const fn name(self) -> &'static str {
        match self {
            Self::Turn => "sh-turn",
            Self::Job => "sh-job",
            Self::Tick => "sh-tick",
            Self::Memory => "sh-memory",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    #[error("shell thread admission refused: {0:?}")]
    Limit(LimitExceeded),
    #[error("could not start a shell thread: {kind}")]
    Spawn { kind: io::ErrorKind },
    #[error(transparent)]
    Task(#[from] TaskFailure),
}

impl Failure {
    pub(crate) fn category(&self) -> &'static str {
        match self {
            Self::Limit(_) => "shell-threads",
            Self::Spawn { .. } => "spawn",
            Self::Task(failure) => failure.category(),
        }
    }
}

pub(crate) struct Owner {
    tasks: Mutex<JoinSet<()>>,
    slots: Arc<Semaphore>,
    pub(crate) maximum: u32,
}

impl Owner {
    pub(crate) fn new(maximum: u32) -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(JoinSet::new()),
            slots: Arc::new(Semaphore::new(maximum as usize)),
            maximum,
        })
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        threads: &ShellThreads,
    ) -> Result<Ticket, LimitExceeded> {
        while let Some(result) = self.tasks.lock().try_join_next() {
            observe(result);
        }
        let slot = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_full| LimitExceeded::ShellThreads {
                maximum: self.maximum,
            })?;
        let [permit] = threads.reserve()?;
        Ok(Ticket {
            owner: Arc::clone(self),
            permit,
            slot,
        })
    }

    pub(crate) async fn run<T: Send + 'static>(
        self: &Arc<Self>,
        threads: &ShellThreads,
        work: Work,
        run: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Failure> {
        let ticket = self.reserve(threads).map_err(Failure::Limit)?;
        let result = ticket
            .spawn(work, run)
            .map_err(|error| Failure::Spawn { kind: error.kind() })?;
        result
            .await
            .map_err(|_closed| TaskFailure::Cancelled)?
            .map_err(Failure::Task)
    }

    pub(crate) async fn drain(&self) {
        while let Some(result) = poll_fn(|cx| self.tasks.lock().poll_join_next(cx)).await {
            observe(result);
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.slots.close();
        self.drain().await;
    }
}

fn observe(result: Result<(), tokio::task::JoinError>) {
    let result = result.map_err(TaskFailure::from);
    if let Err(failure) = result {
        tracing::error!(
            event = "gateway_thread_failed",
            failure = failure.category(),
            error = %failure,
            "gateway.thread.failed"
        );
    }
}

pub(crate) struct Ticket {
    owner: Arc<Owner>,
    permit: ThreadPermit,
    slot: OwnedSemaphorePermit,
}

impl Ticket {
    pub(crate) fn spawn<T: Send + 'static>(
        self,
        work: Work,
        run: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<oneshot::Receiver<Result<T, TaskFailure>>> {
        let Self {
            owner,
            permit,
            slot,
        } = self;
        let span = tracing::Span::current();
        let subscriber = tracing::dispatcher::get_default(Clone::clone);
        let join = permit.spawn_owned(ThreadName::new(work.name()), move || {
            tracing::dispatcher::with_default(&subscriber, || span.in_scope(run))
        })?;
        let (reply, result) = oneshot::channel();
        owner.tasks.lock().spawn(
            async move {
                let _slot = slot;
                let result = join
                    .await
                    .map_err(TaskFailure::from)
                    .and_then(|result| result.map_err(TaskFailure::from));
                match reply.send(result) {
                    Ok(()) | Err(Ok(_)) => {}
                    Err(Err(failure)) => tracing::error!(
                        event = "gateway_thread_failed",
                        thread.name = work.name(),
                        failure = failure.category(),
                        error = %failure,
                        "gateway.thread.failed"
                    ),
                }
            }
            .instrument(tracing::Span::current()),
        );
        Ok(result)
    }
}
