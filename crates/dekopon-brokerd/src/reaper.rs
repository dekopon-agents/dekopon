use std::{future::Future, time::Duration};

use dekopon_storage_host::{RetentionPolicies, StorageHost, StorageHostError, SweepSummary};
use tokio::{task::JoinError, time::MissedTickBehavior};

use crate::{BrokerdError, ServerError};

const SWEEP_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

pub(crate) async fn serve<F>(
    server: F,
    storage: Option<StorageHost>,
    policies: RetentionPolicies,
) -> Result<(), BrokerdError>
where
    F: Future<Output = Result<(), ServerError>>,
{
    let Some(storage) = storage else {
        return server.await.map_err(BrokerdError::from);
    };
    tokio::pin!(server);
    let mut interval = sweep_interval();
    loop {
        tokio::select! {
            biased;
            result = &mut server => return result.map_err(BrokerdError::from),
            _ = interval.tick() => {}
        }
        let host = storage.clone();
        let policies = policies.clone();
        let mut sweep = tokio::task::spawn_blocking(move || host.sweep(&policies));
        tokio::select! {
            result = &mut server => {
                return drain_after_server(result, sweep).await;
            }
            result = &mut sweep => finish(result)?,
        }
    }
}

async fn drain_after_server(
    server: Result<(), ServerError>,
    sweep: tokio::task::JoinHandle<Result<SweepSummary, StorageHostError>>,
) -> Result<(), BrokerdError> {
    // Native deletion must finish before the broker releases its storage root lease.
    let cleanup = finish(sweep.await);
    server?;
    cleanup
}

fn sweep_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

fn finish(
    result: Result<Result<SweepSummary, StorageHostError>, JoinError>,
) -> Result<(), BrokerdError> {
    match result {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => {
            tracing::warn!(
                event = "storage_sweep_failed",
                error = %dekopon_core::error_chain(&error)
            );
            Ok(())
        }
        Err(error) => {
            tracing::error!(event = "storage_sweep_task_failed", error = %error);
            Err(BrokerdError::StorageReaper(error))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use dekopon_capability::{StorageAccess, StorageInterface, StorageRetention, StorageScope};
    use dekopon_storage_host::{ContinuityPolicy, StorageGrantRequest, StorageLimits};
    use tokio::sync::oneshot;

    use super::*;

    fn expired_resource(
        root: &std::path::Path,
    ) -> (StorageHost, RetentionPolicies, std::path::PathBuf) {
        let host = StorageHost::open(root, StorageLimits::default()).unwrap();
        let provider: dekopon_core::ProviderId = "sql".parse().unwrap();
        let request = StorageGrantRequest::new(
            "invocation".parse().unwrap(),
            "sql.exec".parse().unwrap(),
            provider.clone(),
            StorageInterface::DurableFiles,
            StorageAccess::ReadWrite,
            StorageScope::Agent,
            "assistant".parse().unwrap(),
            dekopon_core::ExternalSubject::slack("T123", "U123").unwrap(),
            "slack",
            "work",
            "c123",
            "c123:123.456",
            ContinuityPolicy::Stable,
            vec![],
        );
        drop(host.grant(request).unwrap());
        let namespaces = root.join("namespaces");
        let resource = fs::read_dir(&namespaces)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::File::options()
            .write(true)
            .open(resource.join("last-used"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();
        let policies = RetentionPolicies::from([(
            (provider, StorageScope::Agent),
            StorageRetention::IdleTtl(Duration::from_secs(60)),
        )]);
        (host, policies, resource)
    }

    #[tokio::test]
    async fn a_startup_sweep_reaps_expired_storage_and_serving_still_finishes() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().canonicalize().unwrap().join("storage");
        let (host, policies, resource) = expired_resource(&storage);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(serve(
            async {
                stopped.await.unwrap();
                Ok(())
            },
            Some(host),
            policies,
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while resource.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_stopped_server_does_not_start_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().canonicalize().unwrap().join("storage");
        let (host, policies, resource) = expired_resource(&storage);
        serve(async { Ok(()) }, Some(host), policies).await.unwrap();
        assert!(resource.exists());
    }

    #[tokio::test]
    async fn shutdown_drains_a_started_native_sweep_even_after_server_failure() {
        let (release, released) = std::sync::mpsc::channel();
        let (started, ready) = oneshot::channel();
        let sweep = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv().unwrap();
            Ok(SweepSummary::default())
        });
        ready.await.unwrap();
        let drain = drain_after_server(Err(ServerError::ShutdownTimeout), sweep);
        tokio::pin!(drain);
        tokio::select! {
            biased;
            _ = &mut drain => panic!("shutdown abandoned a native sweep"),
            () = tokio::task::yield_now() => {}
        }
        release.send(()).unwrap();
        assert!(matches!(
            drain.await,
            Err(BrokerdError::Server(ServerError::ShutdownTimeout))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn sweeps_repeat_at_twelve_hours_without_catch_up_bursts() {
        let mut interval = sweep_interval();
        let start = tokio::time::Instant::now();
        assert_eq!(interval.tick().await, start);
        assert_eq!(interval.tick().await, start + SWEEP_INTERVAL);
        tokio::time::advance(SWEEP_INTERVAL * 3).await;
        interval.tick().await;
        assert_eq!(interval.tick().await, start + SWEEP_INTERVAL * 5);
    }

    #[tokio::test]
    async fn no_storage_still_observes_server_errors() {
        let result = serve(
            async { Err(ServerError::ShutdownTimeout) },
            None,
            RetentionPolicies::new(),
        )
        .await;
        assert!(matches!(
            result,
            Err(BrokerdError::Server(ServerError::ShutdownTimeout))
        ));
    }
}
