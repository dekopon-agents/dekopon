//! Shared lifecycle for transient chat-service activity.
//!
//! The coordinator owns sequencing and refresh timing; transport drivers own credentials and API
//! semantics. Activity is presentation only: every failure is isolated from authorization, model
//! execution, history, and terminal reply delivery.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use tokio::{sync::Notify, task::JoinHandle, time::Instant};

use crate::transport::{ActivityTarget, ChatActivity, TransportError};

/// Stop renewing after repeated failures. A later session may try the installation again unless its
/// transport classified the failure as permanent and disabled that native surface itself.
const MAX_CONSECUTIVE_FAILURES: u8 = 2;
const RUNNING: u8 = 0;
const SEALED: u8 = 1;
const FINISHED: u8 = 2;

#[derive(Default)]
struct Coordination {
    state: AtomicU8,
    changed: Notify,
}

impl Coordination {
    fn seal(&self) {
        let _ = self
            .state
            .compare_exchange(RUNNING, SEALED, Ordering::AcqRel, Ordering::Acquire);
        self.changed.notify_one();
    }

    fn finish(&self) {
        self.state.store(FINISHED, Ordering::Release);
        self.changed.notify_one();
    }

    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    async fn changed_from(&self, expected: u8) {
        let changed = self.changed.notified();
        if self.state() == expected {
            changed.await;
        }
    }
}

/// Cloneable control retained by the active-session registry for native Stop events.
#[derive(Clone, Default)]
pub(crate) struct ActivityControl {
    coordination: Option<Arc<Coordination>>,
}

impl ActivityControl {
    /// Stops renewal and queues best-effort cleanup without waiting in the transport reader.
    pub(crate) fn finish(&self) {
        if let Some(coordination) = &self.coordination {
            coordination.finish();
        }
    }
}

/// Session-owned activity generation.
pub(crate) struct ActivityLease {
    coordination: Option<Arc<Coordination>>,
    worker: Option<JoinHandle<()>>,
}

impl ActivityLease {
    /// Starts one generation when both an optional driver and authenticated target are present.
    pub(crate) fn start(
        driver: Option<Arc<dyn ChatActivity>>,
        target: Option<ActivityTarget>,
    ) -> Self {
        let (Some(driver), Some(target)) = (driver, target) else {
            return Self {
                coordination: None,
                worker: None,
            };
        };
        let coordination = Arc::new(Coordination::default());
        let worker = tokio::spawn(run(driver, target, Arc::clone(&coordination)));
        Self {
            coordination: Some(coordination),
            worker: Some(worker),
        }
    }

    /// Gives the routing loop a generation-safe way to finish activity on a native Stop event.
    pub(crate) fn control(&self) -> ActivityControl {
        ActivityControl {
            coordination: self.coordination.clone(),
        }
    }

    /// Synchronously prevents future renewals without waiting on remote cosmetic I/O.
    ///
    /// An already-issued request retains one owner and is allowed to finish under the driver's
    /// short request deadline. Cleanup then follows it in strict per-target order, so Slack cannot
    /// land `processing` after the later `active` transition.
    pub(crate) fn seal(&self) {
        if let Some(coordination) = &self.coordination {
            coordination.seal();
        }
    }

    /// Queues service-specific cleanup after terminal delivery or deliberate silence without
    /// holding session admission.
    pub(crate) fn finish_in_background(&mut self) {
        if let Some(coordination) = self.coordination.take() {
            coordination.finish();
        }
        // Dropping a Tokio JoinHandle detaches rather than aborts. The worker owns the bounded
        // service call and exits after cleanup while the answered session releases admission now.
        self.worker.take();
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        if let Some(coordination) = self.coordination.take() {
            coordination.finish();
        }
    }
}

async fn run(
    driver: Arc<dyn ChatActivity>,
    target: ActivityTarget,
    coordination: Arc<Coordination>,
) {
    let interval = driver.refresh_interval();
    let mut failures = 0_u8;
    let mut show_enabled = true;

    loop {
        match coordination.state() {
            FINISHED => {
                hide(driver.as_ref(), &target).await;
                return;
            }
            SEALED => {
                coordination.changed_from(SEALED).await;
            }
            RUNNING if !show_enabled => {
                coordination.changed_from(RUNNING).await;
            }
            RUNNING => {
                // Do not cancel an issued show call when the session seals. A dropped HTTP future
                // cannot retract bytes already sent, and issuing cleanup before its result creates
                // a remote `active` -> `processing` reordering race. Every real driver applies its
                // own short deadline, so retaining ownership remains bounded.
                match driver.show(target.clone()).await {
                    Ok(()) => failures = 0,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        record_failure("show", &error);
                    }
                }
                if failures >= MAX_CONSECUTIVE_FAILURES {
                    show_enabled = false;
                    continue;
                }
                let Some(interval) = interval else {
                    show_enabled = false;
                    continue;
                };
                let deadline = Instant::now() + interval;
                let changed = coordination.changed.notified();
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => {}
                    () = changed => {}
                }
            }
            _ => unreachable!("activity state is private and closed"),
        }
    }
}

async fn hide(driver: &dyn ChatActivity, target: &ActivityTarget) {
    if let Err(error) = driver.hide(target.clone()).await {
        record_failure("hide", &error);
    }
}

fn record_failure(operation: &'static str, error: &TransportError) {
    tracing::debug!(
        event = "gateway_activity_failed",
        operation,
        category = error.category()
    );
}
