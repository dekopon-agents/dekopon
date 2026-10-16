use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use dekopon_core::SecretDrn;
use tokio::time::MissedTickBehavior;

use crate::{
    BrokerdError, ServerError, ShutdownDeadline,
    secrets::{self, oauth2},
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
    let mut retry = BTreeMap::<SecretDrn, u64>::new();
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
        retry.retain(|drn, _| entries.iter().any(|(key, _, _)| key == drn));
        for (drn, path, has_seed) in entries {
            let seed_on_boot = new_generation && has_seed;
            let now = now_seconds();
            if retry.get(&drn).is_some_and(|until| now < *until) {
                continue;
            }
            let uid = crate::socket::current_uid();
            let inspected =
                tokio::task::spawn_blocking(move || oauth2::due_at(&path, uid, now)).await;
            let due = match inspected {
                Err(_) => false,
                Ok(result) => match result {
                    Ok(Some(due)) => due <= now || seed_on_boot,
                    Ok(None) => seed_on_boot,
                    Err(error) => {
                        tracing::warn!(event = "broker_credential_record_invalid", credential = %drn, category = error.category());
                        false
                    }
                },
            };
            if !due {
                continue;
            }
            let attempt = current.refresh(&drn, Arc::clone(&now_seconds));
            tokio::pin!(attempt);
            tokio::select! {
                biased;
                result = &mut server => {
                    let drain = match deadline.get() {
                        Some(until) => tokio::time::timeout_at(until, attempt).await.is_ok(),
                        None => { drop(attempt.await); true },
                    };
                    if !drain { return Err(BrokerdError::Server(ServerError::ShutdownTimeout)); }
                    return result;
                }
                result = &mut attempt => {
                    if let Err(error) = result {
                        tracing::warn!(event = "broker_credential_scheduled_failed", credential = %drn, category = error.category());
                        retry.insert(drn.clone(), now.saturating_add(oauth2::retry_seconds()));
                    } else {
                        retry.remove(&drn);
                    }
                }
            }
        }
    }
}
