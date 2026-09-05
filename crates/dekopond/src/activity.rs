//! Bounded cosmetic workers. Neither a reply lock nor session admission is held by cosmetic I/O.

use crate::transport::{ActivityTarget, ChatActivity, TransportError};
use dekopon_harness::activity::{ActivityEvent, ActivityLabel, ActivityPublisher};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, Semaphore},
    task::JoinHandle,
    time::Instant,
};

const RUNNING: u8 = 0;
const SEALED: u8 = 1;
const FINISHED: u8 = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(2);
/// Targets a live generation may hold at once, across every transport.
const MAX_ACTIVE_TARGETS: usize = 128;
/// Targets held back because their native ordering is unknown, counted separately from the live
/// ceiling so an accreted quarantine can never refuse a healthy new session its activity.
const MAX_QUARANTINED_TARGETS: usize = 128;
/// How long an uncertain native write keeps its target reserved.
///
/// The reservation exists because a `processing` write that may still land must not be overtaken
/// by a later generation's `active`. Slack does not hold an unacknowledged write for anything like
/// this long, so past it the ordering question is settled and the thread is usable again.
const QUARANTINE_TTL: Duration = Duration::from_secs(15 * 60);
static REQUESTS: Semaphore = Semaphore::const_new(16);
type TargetKey = (usize, ActivityTarget);
static TARGETS: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    TARGETS.get_or_init(Default::default)
}

/// Which targets a generation may take, and which are held back and why.
#[derive(Default)]
struct Registry {
    /// Targets a live generation owns right now.
    active: HashSet<TargetKey>,
    /// Uncertain native targets, each holding the driver allocation so a later installation can
    /// never be handed the same address and inherit this quarantine, plus when it was recorded.
    quarantined: HashMap<TargetKey, (std::sync::Weak<dyn ChatActivity>, Instant)>,
    /// Set once the quarantine is full, so the operator hears about it once rather than per lease.
    quarantine_full_reported: bool,
}
impl Registry {
    fn expire(&mut self, now: Instant) {
        self.quarantined
            .retain(|_, (_, recorded)| now.saturating_duration_since(*recorded) < QUARANTINE_TTL);
        if self.quarantined.len() < MAX_QUARANTINED_TARGETS {
            self.quarantine_full_reported = false;
        }
    }
    /// Takes the target for a new generation, or names the reason it is unavailable.
    fn admit(&mut self, key: &TargetKey, now: Instant) -> Result<(), &'static str> {
        self.expire(now);
        if self.quarantined.contains_key(key) {
            return Err("quarantined");
        }
        if self.active.contains(key) {
            return Err("busy");
        }
        if self.active.len() >= MAX_ACTIVE_TARGETS {
            return Err("capacity");
        }
        self.active.insert(key.clone());
        Ok(())
    }
    fn release(&mut self, key: &TargetKey) {
        self.active.remove(key);
    }
    /// Holds a released target back. Returns whether this call is the one that filled the
    /// quarantine, so exactly one warning is emitted for a ceiling that stays reached.
    fn quarantine(
        &mut self,
        key: &TargetKey,
        driver: std::sync::Weak<dyn ChatActivity>,
        now: Instant,
    ) -> bool {
        self.active.remove(key);
        self.expire(now);
        let mut newly_full = false;
        if self.quarantined.len() >= MAX_QUARANTINED_TARGETS && !self.quarantined.contains_key(key)
        {
            newly_full = !std::mem::replace(&mut self.quarantine_full_reported, true);
            // The oldest reservation is the one whose ordering question is closest to settled.
            if let Some(oldest) = self
                .quarantined
                .iter()
                .min_by_key(|(_, (_, recorded))| *recorded)
                .map(|(key, _)| key.clone())
            {
                self.quarantined.remove(&oldest);
            }
        }
        self.quarantined.insert(key.clone(), (driver, now));
        newly_full
    }
}

struct TargetLease {
    key: TargetKey,
    driver: std::sync::Weak<dyn ChatActivity>,
    quarantine: bool,
}
impl TargetLease {
    fn acquire(
        driver: &Arc<dyn ChatActivity>,
        mut target: ActivityTarget,
    ) -> Result<Self, &'static str> {
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
        registry()
            .lock()
            .expect("activity targets")
            .admit(&key, Instant::now())?;
        Ok(Self {
            key,
            driver: Arc::downgrade(driver),
            quarantine: false,
        })
    }
}
impl Drop for TargetLease {
    fn drop(&mut self) {
        let mut registry = registry().lock().expect("activity targets");
        let newly_full = if self.quarantine {
            registry.quarantine(&self.key, self.driver.clone(), Instant::now())
        } else {
            registry.release(&self.key);
            false
        };
        drop(registry);
        if newly_full {
            tracing::warn!(
                event = "gateway_activity_failed",
                operation = "quarantine",
                cause_type = "activity-quarantine-full"
            );
        }
    }
}

/// The activity supervisors one gateway owns.
///
/// Cleanup of a progress message is the last thing a session's activity does, and it outlives the
/// session task that started it. `serve` drains this so a SIGTERM does not strand a ⌛ message in
/// somebody's channel. It is owned by the gateway rather than by the process because two gateways
/// in one process (or two tests) must not drain — or abandon — each other's workers.
#[derive(Clone, Default)]
pub(crate) struct ActivitySupervisors(Arc<Mutex<Vec<JoinHandle<()>>>>);

impl ActivitySupervisors {
    fn track(&self, handle: JoinHandle<()>) {
        let mut supervisors = self.0.lock().expect("activity supervisors");
        supervisors.retain(|handle| !handle.is_finished());
        supervisors.push(handle);
    }

    /// Awaits every activity supervisor still posting, updating, or cleaning up.
    ///
    /// Called by `serve` after the sessions, inside the same shutdown grace: an activity worker
    /// holds no session admission, so nothing here can be waiting on work the sessions had not
    /// finished.
    pub(crate) async fn drain(&self) {
        // Handles are moved out so nothing awaits under the lock, and put back on the way out: a
        // drain cancelled by the shutdown grace expiring must leave them for `abandon` to count,
        // rather than dropping them and reporting that nothing was stranded.
        struct Restore<'a>(&'a ActivitySupervisors, Vec<JoinHandle<()>>);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.0
                    .0
                    .lock()
                    .expect("activity supervisors")
                    .append(&mut self.1);
            }
        }
        loop {
            let pending = std::mem::take(&mut *self.0.lock().expect("activity supervisors"));
            if pending.is_empty() {
                return;
            }
            let mut remaining = Restore(self, pending);
            // Awaited in place and popped after it finishes, so a cancellation here leaves the
            // handle in the vector the guard puts back rather than dropping it mid-await.
            while let Some(handle) = remaining.1.last_mut() {
                let outcome = handle.await;
                remaining.1.pop();
                if let Err(error) = outcome
                    && !error.is_cancelled()
                {
                    tracing::debug!(
                        event = "gateway_activity_failed",
                        operation = "supervisor",
                        category = "panic",
                        cause = %error
                    );
                }
            }
        }
    }

    /// Gives up on the supervisors the shutdown grace did not cover.
    ///
    /// Each one that was still running owns a ⌛ message it had not finished removing, so the
    /// count is how many progress artifacts this shutdown leaves in somebody's channel. Nothing
    /// else observes an aborted supervisor's exit, so the caller reports the count rather than
    /// letting a stranded artifact be the one shutdown outcome that is invisible.
    pub(crate) fn abandon(&self) -> usize {
        let mut abandoned = 0;
        for handle in std::mem::take(&mut *self.0.lock().expect("activity supervisors")) {
            if !handle.is_finished() {
                abandoned += 1;
            }
            handle.abort();
        }
        abandoned
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
        supervisors: &ActivitySupervisors,
        driver: Option<Arc<dyn ChatActivity>>,
        target: Option<ActivityTarget>,
        optional_reply: bool,
    ) -> Self {
        let (Some(driver), Some(target)) = (driver, target) else {
            return Self { coordination: None };
        };
        let lease = match TargetLease::acquire(&driver, target.clone()) {
            Ok(lease) => lease,
            Err(category) => {
                tracing::debug!(
                    event = "gateway_activity_failed",
                    operation = "lease",
                    category
                );
                return Self { coordination: None };
            }
        };
        let coordination = Arc::new(Coordination::default());
        let worker_coordination = coordination.clone();
        // Self-contained bounded worker owns its target through late creation and cleanup. This
        // supervisor observes panics, and conservatively quarantines uncertain ordering on unwind.
        // The worker rides a `JoinSet` rather than a bare spawn so that abandoning the supervisor
        // abandons the worker with it, and the supervisor itself is owned by the gateway.
        supervisors.track(tokio::spawn(async move {
            let mut lease = lease;
            lease.quarantine = true;
            let mut worker = tokio::task::JoinSet::new();
            worker.spawn(run(driver, target, worker_coordination, optional_reply));
            match worker.join_next().await {
                Some(Ok(quarantine)) => lease.quarantine = quarantine,
                Some(Err(error)) => tracing::debug!(
                    event = "gateway_activity_failed",
                    operation = "worker",
                    category = if error.is_panic() {
                        "panic"
                    } else {
                        "cancelled"
                    },
                    cause = %error
                ),
                None => tracing::debug!(
                    event = "gateway_activity_failed",
                    operation = "worker",
                    category = "cancelled"
                ),
            }
        }));
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

#[cfg(test)]
mod registry_tests {
    use super::*;

    /// A driver whose only job is to give the registry a distinct allocation address.
    struct Fake;
    impl ChatActivity for Fake {
        fn show(
            &self,
            _: ActivityTarget,
        ) -> futures_util::future::BoxFuture<'_, Result<(), TransportError>> {
            Box::pin(async { Ok(()) })
        }
        fn hide(
            &self,
            _: ActivityTarget,
        ) -> futures_util::future::BoxFuture<'_, Result<(), TransportError>> {
            Box::pin(async { Ok(()) })
        }
        fn refresh_interval(&self) -> Option<Duration> {
            None
        }
        fn retire(&self, _: &ActivityTarget) {}
    }
    fn key(index: usize) -> TargetKey {
        (
            index,
            ActivityTarget::Slack {
                channel_id: format!("C{index}"),
                thread_ts: "1700000000.000001".into(),
                message_ts: String::new(),
                initiator_user_id: String::new(),
            },
        )
    }
    fn driver() -> (Arc<dyn ChatActivity>, std::sync::Weak<dyn ChatActivity>) {
        let driver: Arc<dyn ChatActivity> = Arc::new(Fake);
        let weak = Arc::downgrade(&driver);
        (driver, weak)
    }

    #[test]
    fn a_full_quarantine_is_reported_once_and_never_refuses_a_live_lease() {
        let (_driver, weak) = driver();
        let mut registry = Registry::default();
        let now = Instant::now();
        for index in 0..MAX_QUARANTINED_TARGETS {
            assert!(
                !registry.quarantine(&key(index), weak.clone(), now),
                "an unfilled quarantine reports nothing"
            );
        }
        assert!(
            registry.quarantine(&key(MAX_QUARANTINED_TARGETS), weak.clone(), now),
            "the call that finds the quarantine full reports it"
        );
        assert!(
            !registry.quarantine(&key(MAX_QUARANTINED_TARGETS + 1), weak.clone(), now),
            "a quarantine that stays full is reported once, not per lease"
        );
        assert_eq!(registry.quarantined.len(), MAX_QUARANTINED_TARGETS);
        // The whole point of the split: quarantine accretion never disables live activity.
        registry
            .admit(&key(9_000), now)
            .expect("a live target is unaffected by a full quarantine");
    }

    #[test]
    fn a_quarantined_target_ages_out_and_becomes_usable_again() {
        let (_driver, weak) = driver();
        let mut registry = Registry::default();
        let now = Instant::now();
        registry.quarantine(&key(1), weak, now);
        assert_eq!(
            registry.admit(&key(1), now + QUARANTINE_TTL - Duration::from_secs(1)),
            Err("quarantined"),
            "an unsettled native write still holds its thread"
        );
        registry
            .admit(&key(1), now + QUARANTINE_TTL + Duration::from_secs(1))
            .expect("past the age-out the ordering question is settled");
    }

    #[test]
    fn an_unavailable_target_names_whether_it_is_busy_or_at_capacity() {
        let (_driver, weak) = driver();
        let mut registry = Registry::default();
        let now = Instant::now();
        registry.admit(&key(1), now).expect("the first generation");
        assert_eq!(registry.admit(&key(1), now), Err("busy"));
        for index in 2..=MAX_ACTIVE_TARGETS {
            registry
                .admit(&key(index), now)
                .expect("inside the ceiling");
        }
        assert_eq!(registry.admit(&key(9_001), now), Err("capacity"));
        registry.quarantine(&key(1), weak, now);
        assert_eq!(
            registry.admit(&key(1), now),
            Err("quarantined"),
            "a quarantined target is not reported as a live generation holding the thread"
        );
        // Quarantining released the active slot, so the ceiling admits another live target.
        registry.admit(&key(9_001), now).expect("released capacity");
    }
}
