use std::{future::Future, time::Duration};

use dekopon_storage_host::{RetentionPolicies, StorageHost, StorageHostError, SweepSummary};
use tokio::{task::JoinError, time::MissedTickBehavior};

use crate::{BrokerdError, ServerError, ShutdownDeadline};

const SWEEP_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

pub(crate) async fn serve<F, P>(
    server: F,
    storage: Option<StorageHost>,
    policies: P,
    deadline: ShutdownDeadline,
) -> Result<(), BrokerdError>
where
    F: Future<Output = Result<(), ServerError>>,
    P: Fn() -> RetentionPolicies,
{
    let Some(storage) = storage else {
        return server.await.map_err(BrokerdError::from);
    };
    tokio::pin!(server);
    let mut interval = sweep_interval();
    let mut progress = SweepProgress::default();
    loop {
        tokio::select! {
            biased;
            result = &mut server => return result.map_err(BrokerdError::from),
            _ = interval.tick() => {}
        }
        let host = storage.clone();
        let policies = policies();
        let mut sweep = tokio::task::spawn_blocking(move || host.sweep(&policies));
        tokio::select! {
            result = &mut server => {
                return drain_after_server(result, sweep, &deadline, progress).await;
            }
            result = &mut sweep => progress.finish(result)?,
        }
    }
}

async fn drain_after_server(
    server: Result<(), ServerError>,
    sweep: tokio::task::JoinHandle<Result<SweepSummary, StorageHostError>>,
    deadline: &ShutdownDeadline,
    mut progress: SweepProgress,
) -> Result<(), BrokerdError> {
    // Native deletion finishes before the storage root lease is released; past the shutdown
    // deadline the process exits holding both, and the lease dies with it.
    let cleanup = match deadline.get() {
        Some(deadline) => match tokio::time::timeout_at(deadline, sweep).await {
            Ok(result) => progress.finish(result),
            Err(_) => Err(BrokerdError::Server(ServerError::ShutdownTimeout)),
        },
        None => progress.finish(sweep.await),
    };
    server?;
    cleanup
}

fn sweep_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

#[derive(Default)]
struct SweepProgress {
    sequence: u64,
    failures: u64,
}

impl SweepProgress {
    fn finish(
        &mut self,
        result: Result<Result<SweepSummary, StorageHostError>, JoinError>,
    ) -> Result<(), BrokerdError> {
        self.sequence += 1;
        match result {
            Ok(Ok(summary)) if summary.errors > 0 => self.record_failure(summary.errors, None),
            Ok(Ok(_)) => {
                let failures = std::mem::take(&mut self.failures);
                if failures > 0 {
                    tracing::info!(
                        event = "storage_sweep_recovered",
                        sweep.sequence = self.sequence,
                        failed_sweep.count = failures,
                        "storage.sweep.recovered"
                    );
                }
            }
            Ok(Err(error)) => self.record_failure(1, Some(&error)),
            Err(error) => {
                let failure = dekopon_core::TaskOutcome::from_join_error(error);
                tracing::error!(
                    event = "storage_sweep_task_failed",
                    failure = failure.category(),
                    error = %failure,
                    "storage.sweep.task.failed"
                );
                return Err(BrokerdError::StorageReaper(failure));
            }
        }
        Ok(())
    }

    fn record_failure(&mut self, errors: u64, error: Option<&StorageHostError>) {
        self.failures += 1;
        let error = error.map(|error| dekopon_core::error_chain(error));
        tracing::warn!(
            event = "storage_sweep_failed",
            sweep.sequence = self.sequence,
            sweep.error_count = errors,
            error = error.as_deref(),
            "storage.sweep.failed"
        );
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
            dekopon_core::Subject::slack_user("T123", "U123").unwrap(),
            "slack",
            "work",
            "c123",
            "c123:123.456",
            ContinuityPolicy::Relaxed,
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
            move || policies.clone(),
            ShutdownDeadline::default(),
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

    #[test]
    fn each_failed_sweep_pass_is_reported_and_the_next_clean_pass_closes_the_run() {
        use tracing_subscriber::prelude::*;

        let root = tempfile::tempdir().unwrap();
        let storage = root.path().canonicalize().unwrap().join("storage");
        let (host, policies, _) = expired_resource(&storage);
        let bad_entry = storage.join("namespaces").join("bad-entry");
        fs::write(&bad_entry, []).unwrap();
        let capture = dekopon_test_support::CaptureLayer::new();
        tracing::subscriber::with_default(
            tracing_subscriber::registry().with(capture.clone()),
            || {
                let mut progress = SweepProgress::default();
                progress.finish(Ok(Err(StorageHostError::Busy))).unwrap();
                progress.finish(Ok(host.sweep(&policies))).unwrap();
                fs::remove_file(bad_entry).unwrap();
                for _ in 0..2 {
                    progress.finish(Ok(host.sweep(&policies))).unwrap();
                }
            },
        );
        let failed = capture.events_where(
            "dekopon_brokerd::reaper",
            &[("event", "storage_sweep_failed")],
        );
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].field("sweep.sequence"), Some("1"));
        assert_eq!(failed[1].field("sweep.sequence"), Some("2"));
        assert_eq!(failed[1].field("sweep.error_count"), Some("1"));
        let recovered = capture.events_where(
            "dekopon_brokerd::reaper",
            &[("event", "storage_sweep_recovered")],
        );
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].field("sweep.sequence"), Some("3"));
        assert_eq!(recovered[0].field("failed_sweep.count"), Some("2"));
    }

    #[tokio::test]
    async fn a_stopped_server_does_not_start_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().canonicalize().unwrap().join("storage");
        let (host, policies, resource) = expired_resource(&storage);
        serve(
            async { Ok(()) },
            Some(host),
            || policies.clone(),
            ShutdownDeadline::default(),
        )
        .await
        .unwrap();
        assert!(resource.exists());
    }

    fn parked_sweep() -> (
        std::sync::mpsc::Sender<()>,
        oneshot::Receiver<()>,
        tokio::task::JoinHandle<Result<SweepSummary, StorageHostError>>,
    ) {
        let (release, released) = std::sync::mpsc::channel();
        let (started, ready) = oneshot::channel();
        let sweep = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            released.recv().unwrap();
            Ok(SweepSummary::default())
        });
        (release, ready, sweep)
    }

    #[tokio::test]
    async fn a_started_native_sweep_drains_inside_the_shutdown_deadline_even_after_server_failure()
    {
        let (release, ready, sweep) = parked_sweep();
        ready.await.unwrap();
        let deadline = ShutdownDeadline::default();
        deadline.start(Duration::from_secs(60));
        let drain = drain_after_server(
            Err(ServerError::ShutdownTimeout),
            sweep,
            &deadline,
            SweepProgress::default(),
        );
        tokio::pin!(drain);
        tokio::select! {
            biased;
            _ = &mut drain => panic!("shutdown abandoned a native sweep inside its deadline"),
            () = tokio::task::yield_now() => {}
        }
        release.send(()).unwrap();
        assert!(matches!(
            drain.await,
            Err(BrokerdError::Server(ServerError::ShutdownTimeout))
        ));
    }

    #[tokio::test]
    async fn a_sweep_still_running_at_the_shutdown_deadline_fails_the_shutdown() {
        let (release, ready, sweep) = parked_sweep();
        ready.await.unwrap();
        let deadline = ShutdownDeadline::default();
        deadline.start(Duration::from_millis(50));
        let result = drain_after_server(Ok(()), sweep, &deadline, SweepProgress::default()).await;
        release.send(()).unwrap();
        assert!(matches!(
            result,
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
            RetentionPolicies::new,
            ShutdownDeadline::default(),
        )
        .await;
        assert!(matches!(
            result,
            Err(BrokerdError::Server(ServerError::ShutdownTimeout))
        ));
    }
}
