use std::{sync::Arc, time::Duration};

use dekopon_core::TaskOutcome;
use tokio::{sync::oneshot, task::JoinSet};

use crate::{
    BrokerdError, ShutdownDeadline,
    self_reload::{self, BrokerStop},
    tests::{ManagedBroker, stopped_within},
};

fn serve_seeded<F, R>(
    broker: &ManagedBroker,
    shutdown: F,
    task: R,
) -> tokio::task::JoinHandle<Result<(), BrokerdError>>
where
    F: Future<Output = ()> + Send + 'static,
    R: Future<Output = BrokerStop> + Send + 'static,
{
    tokio::spawn(crate::serve_with_reload(
        broker.config(),
        shutdown,
        ShutdownDeadline::default(),
        |_, _| Some(task),
    ))
}

async fn panics() -> BrokerStop {
    panic!("reload task panicked")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_broker_reload_task_stops_the_broker_with_its_cause() {
    let broker = ManagedBroker::new();
    broker.publish_lock('1');
    let task = serve_seeded(&broker, std::future::pending(), panics());

    assert!(matches!(
        stopped_within(task).await,
        Err(BrokerdError::ReloadTask(TaskOutcome::Panicked))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_broker_reload_task_is_joined_at_shutdown() {
    let broker = ManagedBroker::new();
    broker.publish_lock('1');
    let held = Arc::new(());
    let guard = Arc::clone(&held);
    let (stop, stopped) = oneshot::channel::<()>();
    let task = serve_seeded(
        &broker,
        async {
            drop(stopped.await);
        },
        async move {
            let _guard = guard;
            std::future::pending().await
        },
    );
    broker.ready(&task).await;
    stop.send(()).expect("broker still serving");

    stopped_within(task)
        .await
        .expect("a requested shutdown exits successfully");
    assert_eq!(Arc::strong_count(&held), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_panic_outranks_a_drain_that_outlives_the_grace() {
    let broker = ManagedBroker::new();
    broker.publish_lock('1');
    let deadline = ShutdownDeadline::default();
    let (expired, panic_now) = oneshot::channel::<()>();
    let task = tokio::spawn(crate::serve_with_reload(
        broker.config(),
        std::future::pending(),
        deadline.clone(),
        |_, _| {
            Some(async move {
                drop(panic_now.await);
                panics().await
            })
        },
    ));
    broker.ready(&task).await;
    let until = deadline.start(Duration::ZERO);
    tokio::time::sleep_until(until + Duration::from_millis(1)).await;
    expired.send(()).expect("reload task is waiting");

    assert!(matches!(
        stopped_within(task).await,
        Err(BrokerdError::ReloadTask(TaskOutcome::Panicked))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lock_change_through_the_reload_task_ends_the_broker_cleanly() {
    let broker = ManagedBroker::new();
    broker.publish_lock('1');
    let task = broker.serve(std::future::pending());
    broker.ready(&task).await;
    broker.publish_lock('2');

    stopped_within(task)
        .await
        .expect("a lock change drains and exits successfully");
}

#[tokio::test(flavor = "current_thread")]
async fn a_reload_panic_that_races_shutdown_still_stops_the_broker_with_its_cause() {
    let broker = ManagedBroker::new();
    broker.publish_lock('1');
    let (panicking, panicked) = oneshot::channel::<()>();
    let task = serve_seeded(
        &broker,
        async {
            drop(panicked.await);
        },
        async move {
            panicking.send(()).expect("shutdown is waiting");
            panics().await
        },
    );

    assert!(matches!(
        stopped_within(task).await,
        Err(BrokerdError::ReloadTask(TaskOutcome::Panicked))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn a_reload_panic_that_races_shutdown_outranks_a_drain_that_outlives_the_grace() {
    let broker = ManagedBroker::new();
    broker.publish_lock('1');
    let deadline = ShutdownDeadline::default();
    deadline.start(Duration::ZERO);
    let (panicking, panicked) = oneshot::channel::<()>();
    let task = tokio::spawn(crate::serve_with_reload(
        broker.config(),
        async {
            drop(panicked.await);
        },
        deadline,
        |_, _| {
            Some(async move {
                panicking.send(()).expect("shutdown is waiting");
                panics().await
            })
        },
    ));

    assert!(matches!(
        stopped_within(task).await,
        Err(BrokerdError::ReloadTask(TaskOutcome::Panicked))
    ));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_broker_reload_drain_gets_no_fresh_grace() {
    let grace = Duration::from_secs(5);
    let deadline = ShutdownDeadline::default();
    deadline.start(grace);
    tokio::time::advance(grace).await;
    let mut reload = JoinSet::new();
    reload.spawn(std::future::pending::<BrokerStop>());
    let drained_at = tokio::time::Instant::now();

    let drained = self_reload::drain_bounded(&deadline, grace, &mut reload).await;

    assert!(matches!(drained, self_reload::Drained::Elapsed));
    assert_eq!(tokio::time::Instant::now(), drained_at);
}
