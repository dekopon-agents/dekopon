use std::time::Duration;

use dekopon_core::TaskOutcome;
use tokio::{task::JoinSet, time::timeout_at};

use crate::{
    BrokerdError, ShutdownDeadline,
    lock_watch::{self, LoadedLock, LockStop},
};

#[derive(Debug)]
pub(crate) enum BrokerStop {
    LockChanged,
    Failed(BrokerdError),
}

pub(crate) async fn watch_lock(lock: LoadedLock, uid: u32, poll: Duration) -> BrokerStop {
    match lock_watch::watch(&lock, uid, poll).await {
        LockStop::Changed => BrokerStop::LockChanged,
        LockStop::Unsettled => BrokerStop::Failed(BrokerdError::ProviderLockUnsettled),
    }
}

pub(crate) async fn reload_ended(task: &mut JoinSet<BrokerStop>) -> Result<(), BrokerdError> {
    match task.join_next().await {
        None => std::future::pending().await,
        Some(Ok(BrokerStop::LockChanged)) => Ok(()),
        Some(Ok(BrokerStop::Failed(cause))) => Err(cause),
        Some(Err(failure)) => Err(BrokerdError::ReloadTask(TaskOutcome::from(failure))),
    }
}

pub(crate) enum Drained {
    Clean,
    Failed(BrokerdError),
    Elapsed,
}

pub(crate) async fn drain_bounded(
    deadline: &ShutdownDeadline,
    grace: Duration,
    task: &mut JoinSet<BrokerStop>,
) -> Drained {
    let until = deadline.start(grace);
    match timeout_at(until, drain_reload(task)).await {
        Ok(Ok(())) => Drained::Clean,
        Ok(Err(cause)) => Drained::Failed(cause),
        Err(_) => Drained::Elapsed,
    }
}

async fn drain_reload(task: &mut JoinSet<BrokerStop>) -> Result<(), BrokerdError> {
    task.abort_all();
    let mut ended = Ok(());
    while let Some(result) = task.join_next().await {
        let cause = match result {
            Ok(BrokerStop::LockChanged) => continue,
            Ok(BrokerStop::Failed(cause)) => cause,
            Err(failure) if failure.is_cancelled() => continue,
            Err(failure) => BrokerdError::ReloadTask(TaskOutcome::from(failure)),
        };
        if ended.is_ok() {
            ended = Err(cause);
        }
    }
    ended
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    dead_code,
    reason = "skeleton: daemon/broker-self-reload fills and calls these"
)]
pub(crate) struct GenerationDigest([u8; 32]);

#[expect(
    clippy::todo,
    dead_code,
    reason = "skeleton: daemon/broker-self-reload fills and calls this"
)]
impl GenerationDigest {
    pub(crate) fn of(_candidate_digest: &str) -> Option<Self> {
        todo!()
    }
}

#[expect(
    dead_code,
    reason = "skeleton: daemon/broker-self-reload fills and calls these"
)]
pub(crate) struct CommitSignal {
    latest: tokio::sync::watch::Sender<GenerationDigest>,
}

#[expect(
    clippy::todo,
    dead_code,
    reason = "skeleton: daemon/broker-self-reload fills and calls these"
)]
impl CommitSignal {
    pub(crate) fn new(_boot: GenerationDigest) -> Self {
        todo!()
    }

    pub(crate) fn publish(&self, _committed: GenerationDigest) {
        todo!()
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<GenerationDigest> {
        todo!()
    }
}

#[expect(
    clippy::todo,
    dead_code,
    reason = "skeleton: daemon/broker-self-reload fills and calls this"
)]
pub(crate) async fn check_lock_then_exit(
    _lock: LoadedLock,
    _uid: u32,
    _poll: Duration,
) -> BrokerStop {
    todo!()
}
