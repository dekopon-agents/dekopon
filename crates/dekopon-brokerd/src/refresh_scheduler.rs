use std::{collections::BTreeMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use tokio::{task::JoinSet, time::MissedTickBehavior};
use tracing::instrument::WithSubscriber as _;

use crate::{
    BrokerdError, ServerError, ShutdownDeadline,
    secrets::{self, SourceError, oauth2},
};

const TICK: Duration = Duration::from_secs(5 * 60);

pub(crate) async fn serve<F, C>(
    server: F,
    catalog: C,
    deadline: ShutdownDeadline,
) -> Result<(), BrokerdError>
where
    F: Future<Output = Result<(), BrokerdError>>,
    C: Fn() -> secrets::ScheduledCatalog,
{
    serve_with_clock(server, catalog, deadline, Arc::new(oauth2::now_seconds)).await
}

pub(crate) async fn serve_with_clock<F, C>(
    server: F,
    catalog: C,
    deadline: ShutdownDeadline,
    now_seconds: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> Result<(), BrokerdError>
where
    F: Future<Output = Result<(), BrokerdError>>,
    C: Fn() -> secrets::ScheduledCatalog,
{
    tokio::pin!(server);
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut retry = BTreeMap::<PathBuf, u64>::new();
    let mut attempts = JoinSet::<Result<(), SourceError>>::new();
    let mut generation: Option<secrets::ScheduledCatalog> = None;
    loop {
        tokio::select! {
            biased;
            result = &mut server => return result,
            _ = interval.tick() => {}
        }
        let current = catalog();
        let new_generation = generation
            .as_ref()
            .is_none_or(|previous| !current.same_generation(previous));
        if new_generation {
            retry.clear();
        }
        generation = Some(current.clone());
        let entries = current.entries();
        retry.retain(|record, _| entries.iter().any(|(_, path, _)| path == record));
        for (drn, path, has_seed) in entries {
            let seed_on_boot = new_generation && has_seed;
            let now = now_seconds();
            if retry.get(&path).is_some_and(|until| now < *until) {
                continue;
            }
            let uid = crate::socket::current_uid();
            let record = path.clone();
            let inspected =
                tokio::task::spawn_blocking(move || oauth2::due_at(&record, uid, now)).await;
            let due = match inspected {
                Ok(Ok(Some(due))) => due <= now || seed_on_boot,
                Ok(Ok(None)) => seed_on_boot,
                Ok(Err(_)) | Err(_) => false,
            };
            if !due {
                continue;
            }
            attempts.spawn(
                current
                    .refresh(&drn, Arc::clone(&now_seconds))
                    .with_current_subscriber(),
            );
            tokio::select! {
                biased;
                result = &mut server => {
                    let drained = match deadline.get() {
                        Some(until) => tokio::time::timeout_at(until, attempts.join_next()).await.is_ok(),
                        None => { attempts.join_next().await; true },
                    };
                    if !drained {
                        // The endpoint may already have spent this rotation, so the deadline waits for it instead of aborting.
                        while attempts.join_next().await.is_some() {}
                        result?;
                        return Err(BrokerdError::Server(ServerError::ShutdownTimeout));
                    }
                    return result;
                }
                Some(joined) = attempts.join_next() => {
                    match joined.unwrap_or(Err(SourceError::Internal)) {
                        Ok(()) => { retry.remove(&path); }
                        Err(error) => {
                            tracing::warn!(event = "broker_credential_scheduled_failed", credential = %drn, category = error.category());
                            retry.insert(path, now.saturating_add(oauth2::retry_seconds()));
                        }
                    }
                }
            }
        }
    }
}
