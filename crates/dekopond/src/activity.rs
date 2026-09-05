//! Bounded cosmetic workers. Neither a reply lock nor session admission is held by cosmetic I/O.

use crate::transport::{ActivityTarget, ChatActivity, TransportError};
use dekopon_harness::activity::{ActivityEvent, ActivityLabel, ActivityPublisher};
use std::{
    collections::HashSet,
    future::Future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, Semaphore},
    time::Instant,
};

const RUNNING: u8 = 0;
const SEALED: u8 = 1;
const FINISHED: u8 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
static REQUESTS: Semaphore = Semaphore::const_new(16);
// Active/cleanup targets and quarantined uncertain native targets share the same hard ceiling.
static TARGETS: OnceLock<Mutex<HashSet<(usize, ActivityTarget)>>> = OnceLock::new();

struct TargetLease {
    key: (usize, ActivityTarget),
    quarantine: bool,
}
impl TargetLease {
    fn acquire(driver: &Arc<dyn ChatActivity>, mut target: ActivityTarget) -> Option<Self> {
        // Agent status is thread-global, not sender/message scoped. Never allow an older `active`
        // to land after a newer generation's `processing`. Busy targets degrade to no activity.
        if let ActivityTarget::Slack {
            message_ts,
            initiator_user_id,
            ..
        } = &mut target
        {
            message_ts.clear();
            initiator_user_id.clear();
        }
        let key = (Arc::as_ptr(driver) as *const () as usize, target);
        let mut targets = TARGETS
            .get_or_init(Default::default)
            .lock()
            .expect("activity targets");
        if targets.len() >= 128 || !targets.insert(key.clone()) {
            return None;
        }
        Some(Self {
            key,
            quarantine: false,
        })
    }
}
impl Drop for TargetLease {
    fn drop(&mut self) {
        if !self.quarantine {
            TARGETS
                .get_or_init(Default::default)
                .lock()
                .expect("activity targets")
                .remove(&self.key);
        }
    }
}

#[derive(Default)]
struct Coordination {
    state: AtomicU8,
    changed: Notify,
    publisher: ActivityPublisher,
}
impl Coordination {
    fn seal(&self) {
        self.publisher.seal();
        self.state.fetch_max(SEALED, Ordering::AcqRel);
        self.changed.notify_one();
    }
    fn finish(&self) {
        self.seal();
        self.state.store(FINISHED, Ordering::Release);
        self.changed.notify_one();
    }
    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
}
#[derive(Clone, Default)]
pub(crate) struct ActivityControl {
    coordination: Option<Arc<Coordination>>,
}
impl ActivityControl {
    pub(crate) fn finish(&self) {
        if let Some(c) = &self.coordination {
            c.finish();
        }
    }
}

pub(crate) struct ActivityLease {
    coordination: Option<Arc<Coordination>>,
}
impl ActivityLease {
    pub(crate) fn start(
        driver: Option<Arc<dyn ChatActivity>>,
        target: Option<ActivityTarget>,
        optional_reply: bool,
    ) -> Self {
        let (Some(driver), Some(target)) = (driver, target) else {
            return Self { coordination: None };
        };
        let Some(lease) = TargetLease::acquire(&driver, target.clone()) else {
            tracing::debug!(
                event = "gateway_activity_failed",
                operation = "lease",
                category = "busy-or-capacity"
            );
            return Self { coordination: None };
        };
        let coordination = Arc::new(Coordination::default());
        let worker_coordination = coordination.clone();
        // Self-contained bounded worker owns its target through late creation and cleanup. This
        // supervisor observes panics, and conservatively quarantines uncertain ordering on unwind.
        tokio::spawn(async move {
            let mut lease = lease;
            lease.quarantine = true;
            let worker = tokio::spawn(run(driver, target, worker_coordination, optional_reply));
            match worker.await {
                Ok(quarantine) => lease.quarantine = quarantine,
                Err(error) => tracing::debug!(
                    event = "gateway_activity_failed",
                    operation = "worker",
                    category = if error.is_panic() {
                        "panic"
                    } else {
                        "cancelled"
                    }
                ),
            }
        });
        Self {
            coordination: Some(coordination),
        }
    }
    pub(crate) fn publisher(&self) -> Option<ActivityPublisher> {
        self.coordination.as_ref().map(|c| c.publisher.clone())
    }
    pub(crate) fn control(&self) -> ActivityControl {
        ActivityControl {
            coordination: self.coordination.clone(),
        }
    }
    pub(crate) fn seal(&self) {
        if let Some(c) = &self.coordination {
            c.seal();
        }
    }
    pub(crate) fn finish_in_background(&mut self) {
        if let Some(c) = self.coordination.take() {
            c.finish();
        }
    }
}
impl Drop for ActivityLease {
    fn drop(&mut self) {
        self.finish_in_background();
    }
}

async fn request<T>(
    future: impl Future<Output = Result<T, TransportError>>,
) -> Result<T, TransportError> {
    let _permit = REQUESTS.try_acquire().map_err(|error| {
        tracing::debug!(
            event = "gateway_activity_failed",
            operation = "request-slot",
            category = if matches!(error, tokio::sync::TryAcquireError::Closed) {
                "closed"
            } else {
                "capacity"
            }
        );
        TransportError::ActivityRateLimited {
            retry_after: Duration::from_secs(1),
        }
    })?;
    tokio::time::timeout(REQUEST_TIMEOUT, future).await.map_err(|error| {
        tracing::debug!(event="gateway_activity_failed", operation="request", category="timeout", cause_type=%error);
        TransportError::Service { code: "activity-timeout".into() }
    })?
}
pub(crate) fn uncertain(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Request(_)
            | TransportError::Response
            | TransportError::MalformedResponse(_)
    ) || matches!(error, TransportError::Service { code } if code == "activity-timeout")
}
fn delay(error: &TransportError) -> Duration {
    match error {
        TransportError::ActivityRateLimited { retry_after } => *retry_after,
        _ => PROGRESS_INTERVAL,
    }
}
fn failure(operation: &'static str, error: &TransportError) {
    tracing::debug!(
        event = "gateway_activity_failed",
        operation,
        category = error.category()
    );
}

async fn run(
    driver: Arc<dyn ChatActivity>,
    target: ActivityTarget,
    c: Arc<Coordination>,
    optional: bool,
) -> bool {
    let mut quarantine = false;
    let mut native_due = Some(Instant::now());
    let mut native_started = false;
    let mut native_failures = 0;
    let mut progress_due = Instant::now();
    let mut progress_enabled = driver.progress_enabled();
    let mut post_attempted = false;
    let mut artifact = None;
    let mut latest: Option<ActivityEvent> = None;
    let mut pending_label = (!optional).then(|| ActivityLabel::sanitized("Working"));
    loop {
        let changed = c.changed.notified();
        match c.state() {
            FINISHED => break,
            SEALED => {
                changed.await;
                continue;
            }
            _ => {}
        }
        if let Some(event) = c.publisher.latest() {
            let newer = latest.as_ref().is_none_or(|prior| {
                prior.job == event.job
                    && prior.generation == event.generation
                    && event.sequence > prior.sequence
            });
            if newer {
                // Even a coalesced completion means capability submission occurred; optional
                // continuations may now report, but generic inference alone never posts.
                if latest
                    .as_ref()
                    .is_none_or(|prior| prior.label != event.label)
                {
                    pending_label = Some(event.label.clone());
                }
                latest = Some(event);
            }
        }
        if native_due.is_some_and(|due| due <= Instant::now()) && c.state() == RUNNING {
            native_started = true;
            match request(driver.show(target.clone())).await {
                Ok(()) => {
                    native_failures = 0;
                    native_due = driver.refresh_interval().map(|d| Instant::now() + d);
                }
                Err(error) => {
                    quarantine |= uncertain(&error);
                    native_failures += 1;
                    failure("show", &error);
                    native_due = if native_failures >= 2 || quarantine {
                        None
                    } else {
                        Some(Instant::now() + delay(&error))
                    };
                }
            }
        }
        if progress_enabled
            && pending_label.is_some()
            && progress_due <= Instant::now()
            && c.state() == RUNNING
        {
            let label = pending_label.take().expect("pending label");
            if !post_attempted {
                post_attempted = true; // No retries or searches, including unknown creation.
                match request(driver.post_progress(target.clone(), label)).await {
                    Ok(owned) => {
                        artifact = owned;
                        progress_enabled = artifact.is_some();
                    }
                    Err(error) => {
                        failure("post", &error);
                        progress_enabled = false;
                    }
                }
            } else if let Some(owned) = artifact.as_ref() {
                match request(driver.update_progress(owned, label.clone())).await {
                    Ok(()) => {}
                    Err(error) => {
                        failure("update", &error);
                        if matches!(error, TransportError::ActivityRateLimited { .. }) {
                            pending_label = Some(label);
                            progress_due = Instant::now() + delay(&error);
                        } else {
                            progress_enabled = false;
                        }
                    }
                }
            }
            progress_due = progress_due.max(Instant::now() + PROGRESS_INTERVAL);
        }
        if c.state() != RUNNING {
            continue;
        }
        let due = native_due
            .into_iter()
            .chain((progress_enabled && pending_label.is_some()).then_some(progress_due))
            .min();
        tokio::select! {
            () = changed => {},
            () = c.publisher.changed() => {},
            () = async { if let Some(due) = due { tokio::time::sleep_until(due).await; } else { std::future::pending::<()>().await; } } => {},
        }
    }
    // Ten seconds total; each owned surface receives at most two attempts. A Retry-After that
    // cannot fit leaves a documented residual artifact instead of violating the platform limit.
    let deadline = Instant::now() + Duration::from_secs(10);
    if let Some(owned) = artifact.as_ref() {
        cleanup(|| driver.delete_progress(owned), deadline, "delete").await;
    }
    if native_started {
        quarantine |= cleanup(|| driver.hide(target.clone()), deadline, "hide").await;
        driver.retire(&target);
    }
    quarantine
}
async fn cleanup<F, Fut>(mut action: F, deadline: Instant, operation: &'static str) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), TransportError>>,
{
    let mut unknown = false;
    for attempt in 0..2 {
        if Instant::now() + REQUEST_TIMEOUT > deadline {
            break;
        }
        match request(action()).await {
            Ok(()) => return unknown,
            Err(error) => {
                unknown |= uncertain(&error);
                failure(operation, &error);
                if attempt == 1
                    || matches!(&error, TransportError::Service { code } if matches!(code.as_str(), "missing_scope" | "invalid_auth" | "token_revoked" | "cant_delete_message" | "not_allowed_token_type"))
                {
                    break;
                }
                let next = Instant::now() + delay(&error);
                if next + REQUEST_TIMEOUT > deadline {
                    break;
                }
                tokio::time::sleep_until(next).await;
            }
        }
    }
    unknown
}
