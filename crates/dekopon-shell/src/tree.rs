use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use crate::limits::LimitExceeded;

#[derive(Clone, Debug)]
pub struct CallBudget {
    used: Arc<AtomicU32>,
    maximum: u32,
}

impl CallBudget {
    #[must_use]
    pub fn new(maximum: u32) -> Self {
        Self {
            used: Arc::new(AtomicU32::new(0)),
            maximum,
        }
    }

    #[must_use]
    pub fn used(&self) -> u32 {
        self.used.load(Ordering::Relaxed)
    }

    #[must_use]
    pub const fn maximum(&self) -> u32 {
        self.maximum
    }

    pub(crate) fn charge(&self) -> Result<(), LimitExceeded> {
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                (used < self.maximum).then_some(used + 1)
            })
            .map(|_| ())
            .map_err(|_used| LimitExceeded::CapabilityCalls {
                maximum: self.maximum,
            })
    }
}

#[derive(Clone, Debug)]
pub struct TreeContext {
    deadline: Instant,
    timeout: Duration,
    calls: CallBudget,
}

impl TreeContext {
    #[must_use]
    pub fn new(timeout: Duration, calls: CallBudget) -> Self {
        let started = Instant::now();
        Self {
            deadline: started.checked_add(timeout).unwrap_or(started),
            timeout,
            calls,
        }
    }

    pub(crate) fn check_deadline(&self) -> Result<(), LimitExceeded> {
        if Instant::now() >= self.deadline {
            Err(LimitExceeded::Deadline {
                timeout_ms: self.timeout.as_millis(),
            })
        } else {
            Ok(())
        }
    }

    #[must_use]
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    #[must_use]
    pub(crate) fn calls(&self) -> &CallBudget {
        &self.calls
    }
}
