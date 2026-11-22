use std::time::Duration;

use dekopon_core::TaskOutcome;
use tokio::{task::JoinSet, time::timeout};

use crate::{GatewaydError, ServeOutcome};

pub(super) async fn reload_ended(task: &mut JoinSet<GatewaydError>) -> ServeOutcome {
    match task.join_next().await {
        None => std::future::pending().await,
        Some(Ok(cause)) => ServeOutcome::ReloadStopped(Box::new(cause)),
        Some(Err(failure)) => ServeOutcome::ReloadPanicked(TaskOutcome::from(failure)),
    }
}

pub(super) async fn drain_bounded(
    grace: Duration,
    task: &mut JoinSet<GatewaydError>,
) -> Option<ServeOutcome> {
    timeout(grace, drain_reload(task))
        .await
        .unwrap_or(Some(ServeOutcome::ReloadPanicked(TaskOutcome::Cancelled)))
}

async fn drain_reload(task: &mut JoinSet<GatewaydError>) -> Option<ServeOutcome> {
    task.abort_all();
    let mut ended = None;
    while let Some(result) = task.join_next().await {
        let outcome = match result {
            Ok(cause) => ServeOutcome::ReloadStopped(Box::new(cause)),
            Err(failure) if failure.is_cancelled() => continue,
            Err(failure) => ServeOutcome::ReloadPanicked(TaskOutcome::from(failure)),
        };
        ended.get_or_insert(outcome);
    }
    ended
}
