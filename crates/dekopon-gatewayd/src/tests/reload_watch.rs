use std::{collections::BTreeMap, sync::Arc, time::Duration};

use dekopon_core::TaskOutcome;
use tokio::sync::{mpsc, oneshot};

use super::{NO_STOP_WORDS, idle_routing_loop, temporary};
use crate::{GatewaydError, ServeOutcome, gateway::Gateway};

async fn serve_seeded<F, T>(shutdown: F, task: T) -> ServeOutcome
where
    F: Future<Output = ()> + Send,
    T: Future<Output = GatewaydError> + Send + 'static,
{
    let directory = temporary();
    let (runner, routes) = idle_routing_loop(directory.path()).await;
    let (_sender, receiver) = mpsc::channel(4);
    Gateway::new(
        runner,
        routes,
        Arc::new(BTreeMap::new()),
        Arc::new(BTreeMap::new()),
        Arc::new(NO_STOP_WORDS.to_vec()),
    )
    .seed_reload(task)
    .serve(
        receiver,
        shutdown,
        Duration::from_secs(1),
        crate::collection::Collector::new(&[], 4),
    )
    .await
}

async fn panics() -> GatewaydError {
    panic!("reload task panicked")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_reload_task_stops_the_gateway_with_its_cause() {
    let outcome = serve_seeded(std::future::pending(), panics()).await;

    assert!(matches!(
        outcome,
        ServeOutcome::ReloadPanicked(TaskOutcome::Panicked)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_reload_task_stops_the_gateway_with_its_cause() {
    let outcome = serve_seeded(std::future::pending(), async {
        GatewaydError::BrokerGeneration
    })
    .await;

    assert!(matches!(
        outcome,
        ServeOutcome::ReloadStopped(cause) if matches!(*cause, GatewaydError::BrokerGeneration)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reload_task_is_joined_at_shutdown() {
    let held = Arc::new(());
    let guard = Arc::clone(&held);
    let outcome = serve_seeded(std::future::ready(()), async move {
        let _guard = guard;
        std::future::pending().await
    })
    .await;

    assert!(matches!(outcome, ServeOutcome::Shutdown));
    assert_eq!(Arc::strong_count(&held), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn a_reload_panic_that_races_shutdown_still_stops_the_gateway_with_its_cause() {
    let (panicking, panicked) = oneshot::channel::<()>();
    let outcome = serve_seeded(
        async {
            drop(panicked.await);
        },
        async move {
            panicking.send(()).expect("shutdown is waiting");
            panics().await
        },
    )
    .await;

    assert!(matches!(
        outcome,
        ServeOutcome::ReloadPanicked(TaskOutcome::Panicked)
    ));
}
