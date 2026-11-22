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
