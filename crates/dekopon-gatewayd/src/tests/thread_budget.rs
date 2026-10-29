use super::*;
use crate::threads::{Failure, Owner, Work};
use dekopon_process::{LimitExceeded, ShellThreads};

fn with_thread_cap(runner: Arc<SessionRunner>, maximum: u32) -> Arc<SessionRunner> {
    let mut runner = Arc::into_inner(runner).expect("fresh runner");
    runner.shell_threads = ShellThreads::new(maximum, tokio::runtime::Handle::current());
    runner.thread_owner = Owner::new(maximum);
    runner.jobs = Arc::new(crate::jobs::Jobs::new(2, Arc::clone(&runner.thread_owner)));
    Arc::new(runner)
}

#[tokio::test]
async fn a_turn_at_the_shell_thread_cap_is_refused_without_holding_a_permit() {
    let directory = temporary();
    let (broker, _observed) =
        stub_broker(directory.path(), listings(1, &["cli-probe.upper"])).await;
    let models = ModelScript::new([answer("must not run")]);
    let runner = with_thread_cap(runner(broker, Arc::clone(&models), 1), 1);
    let held = runner.shell_threads.reserve::<1>().unwrap();
    let refusal = runner
        .thread_owner
        .run(&runner.shell_threads, Work::Turn, || ())
        .await;
    assert!(matches!(
        refusal,
        Err(Failure::Limit(LimitExceeded::ShellThreads { maximum: 1 }))
    ));
    let driver = Arc::new(RecordingDriver::default());
    run_session(
        Arc::clone(&runner),
        route(model_config()),
        message("refused"),
        driver.clone(),
    )
    .await;
    assert_eq!(models.requests(), 0);
    assert_eq!(driver.replies().len(), 1);
    drop(held);
    assert!(runner.shell_threads.reserve::<1>().is_ok());
    assert!(runner.thread_owner.reserve(&runner.shell_threads).is_ok());
    runner.thread_owner.shutdown().await;
}

#[tokio::test]
async fn a_job_at_the_shell_thread_cap_is_refused_without_a_row_or_permit() {
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), Vec::new()).await;
    let runner = with_thread_cap(runner(broker, ModelScript::new([]), 1), 1);
    let inbound = message("job");
    let owner = crate::jobs::JobOwner::of(&inbound);
    let anchor = crate::wake::Anchor::for_job(&inbound, &route(model_config()).agent).unwrap();
    let mut notices = runner.jobs.take_notices();
    let held = runner.shell_threads.reserve::<1>().unwrap();
    let refusal = runner.jobs.admit(
        owner.clone(),
        anchor,
        None,
        Arc::from("true"),
        &runner.shell_threads,
    );
    assert!(matches!(
        refusal,
        Err(dekopon_shell::JobRefusal::Limit(
            LimitExceeded::ShellThreads { maximum: 1 }
        ))
    ));
    assert!(runner.jobs.list(&owner).is_empty());
    assert!(notices.try_recv().is_err());
    assert_eq!(runner.jobs.free_permits(), 2);
    drop(held);
    assert!(runner.shell_threads.reserve::<1>().is_ok());
    assert!(runner.thread_owner.reserve(&runner.shell_threads).is_ok());
    runner.thread_owner.shutdown().await;
}

#[tokio::test]
async fn a_tick_at_the_shell_thread_cap_is_refused_without_holding_admission() {
    use tracing_subscriber::prelude::*;
    let directory = temporary();
    let (broker, observed) = stub_broker(directory.path(), Vec::new()).await;
    let runner = with_thread_cap(runner(broker, ModelScript::new([]), 1), 1);
    let store = wakes::wake_store(directory.path());
    let now = std::time::SystemTime::now();
    let inbound = wakes::slack_message("tick");
    let anchor = crate::wake::Anchor::from_inbound(&inbound, &route(model_config()).agent).unwrap();
    let id = store
        .register(
            anchor,
            "wait".to_owned(),
            now,
            crate::wake::store::Schedule::Watch {
                probe: crate::wake::Probe::scripted_for_test("exit 1"),
                every: Duration::from_secs(60),
                until: Duration::from_secs(600),
            },
        )
        .unwrap()
        .id;
    let due = now + Duration::from_secs(61);
    let tick = store.take_due(due).unwrap().ticks.pop().unwrap();
    let capture = dekopon_test_support::CaptureLayer::workspace();
    let subscriber = tracing::Dispatch::new(tracing_subscriber::registry().with(capture.clone()));
    let held = runner.shell_threads.reserve::<1>().unwrap();
    let mut ticks = tokio::task::JoinSet::new();
    let refusal = tracing::dispatcher::with_default(&subscriber, || {
        crate::spawn_tick(&runner, &RoutingTable::default(), &store, &mut ticks, tick)
    });
    assert!(matches!(
        refusal,
        Err(Failure::Limit(LimitExceeded::ShellThreads { maximum: 1 }))
    ));
    assert!(ticks.is_empty());
    assert!(observed.try_recv().is_err());
    assert!(
        runner
            .gate
            .admit_probe(("wake".to_owned(), id.to_string()))
            .is_some()
    );
    let records = capture.records();
    let refused = records
        .iter()
        .filter(|record| record.field("event") == Some("gateway_wake_tick_skipped"))
        .collect::<Vec<_>>();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].field("reason"), Some("shell-threads"));
    assert_eq!(refused[0].field("limit.maximum"), Some("1"));
    assert!(store.take_due(due).unwrap().ticks.is_empty());
    assert_eq!(
        store
            .take_due(due + Duration::from_secs(60))
            .unwrap()
            .ticks
            .len(),
        1
    );
    drop(held);
    assert!(runner.shell_threads.reserve::<1>().is_ok());
    assert!(runner.thread_owner.reserve(&runner.shell_threads).is_ok());
    runner.thread_owner.shutdown().await;
}

#[tokio::test]
async fn a_panicking_job_returns_its_thread_permit_and_its_owner_observes_panicked() {
    use dekopon_process::{ExitCode, JobOutcome};
    let directory = temporary();
    let (broker, _observed) = stub_broker(directory.path(), Vec::new()).await;
    let runner = with_thread_cap(runner(broker, ModelScript::new([]), 1), 1);
    let inbound = message("panic");
    let owner = crate::jobs::JobOwner::of(&inbound);
    let anchor = crate::wake::Anchor::for_job(&inbound, &route(model_config()).agent).unwrap();
    let mut notices = runner.jobs.take_notices();
    let crate::jobs::Admitted { run, thread, .. } = runner
        .jobs
        .admit(
            owner.clone(),
            anchor,
            None,
            Arc::from("panic fixture"),
            &runner.shell_threads,
        )
        .unwrap();
    let result = thread
        .spawn(Work::Job, move || {
            let _run = run;
            panic!("job thread panicked");
        })
        .unwrap();
    assert!(
        matches!(result.await.unwrap(), Err(crate::session::TaskFailure::Panicked { cause }) if cause == "job thread panicked")
    );
    let rows = runner.jobs.list(&owner);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        jobs_e2e::finished(&rows[0]),
        (JobOutcome::Failed, ExitCode::FAILURE)
    );
    assert!(notices.try_recv().is_ok());
    assert!(notices.try_recv().is_err());
    runner.thread_owner.shutdown().await;
    assert_eq!(runner.jobs.free_permits(), 2);
    assert!(runner.shell_threads.reserve::<1>().is_ok());
}

#[tokio::test]
async fn shutdown_joins_admitted_threads_even_after_their_callers_are_cancelled() {
    use std::{
        future::Future as _,
        sync::atomic::{AtomicUsize, Ordering},
        task::Poll,
    };
    let threads = ShellThreads::new(4, tokio::runtime::Handle::current());
    let owner = Owner::new(4);
    let completed = Arc::new(AtomicUsize::new(0));
    let mut releases = Vec::new();
    for work in [Work::Turn, Work::Job, Work::Tick, Work::Memory] {
        let (release, released) = std::sync::mpsc::sync_channel(1);
        let (entered, running) = tokio::sync::oneshot::channel();
        let completed = Arc::clone(&completed);
        let result = owner
            .reserve(&threads)
            .unwrap()
            .spawn(work, move || {
                entered.send(()).unwrap_or(());
                released
                    .recv_timeout(Duration::from_secs(5))
                    .expect("shutdown releases native work");
                completed.fetch_add(1, Ordering::Release);
            })
            .unwrap();
        drop(result);
        running.await.unwrap();
        releases.push(release);
    }
    let mut shutdown = Box::pin(owner.shutdown());
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert_eq!(completed.load(Ordering::Acquire), 0);
    for release in releases {
        release.send(()).unwrap();
    }
    shutdown.await;
    assert_eq!(completed.load(Ordering::Acquire), 4);
    assert!(threads.reserve::<4>().is_ok());
}

#[tokio::test]
async fn a_cancelled_session_task_keeps_its_cancelled_category() {
    let task = tokio::spawn(std::future::pending::<()>());
    task.abort();
    let failure = crate::session::TaskFailure::from(task.await.unwrap_err());
    assert!(matches!(failure, crate::session::TaskFailure::Cancelled));
    assert_eq!(failure.category(), "cancelled");
}
