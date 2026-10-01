use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::limits::{LimitExceeded, Limits};

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
    steps: Arc<AtomicU64>,
    retained: Arc<AtomicU64>,
    limits: Limits,
}

#[derive(Debug)]
#[must_use]
pub struct RetainedBytes {
    used: Arc<AtomicU64>,
    bytes: u64,
    maximum: u64,
}

impl RetainedBytes {
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn shrink(&mut self, bytes: u64) {
        debug_assert!(bytes <= self.bytes);
        self.bytes -= bytes;
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }

    pub(crate) fn grow(&mut self, bytes: u64) -> Result<(), LimitExceeded> {
        self.used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|next| *next <= self.maximum)
            })
            .map_err(|_used| LimitExceeded::ValueBytes {
                maximum: self.maximum,
            })?;
        self.bytes += bytes;
        Ok(())
    }
}

impl Drop for RetainedBytes {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

impl TreeContext {
    #[must_use]
    pub fn new(limits: Limits, calls: CallBudget) -> Self {
        let started = Instant::now();
        Self {
            deadline: started.checked_add(limits.timeout).unwrap_or(started),
            timeout: limits.timeout,
            calls,
            steps: Arc::new(AtomicU64::new(0)),
            retained: Arc::new(AtomicU64::new(0)),
            limits,
        }
    }

    pub(crate) fn charge_step(&self) -> Result<(), LimitExceeded> {
        self.steps
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(1)
                    .filter(|next| *next <= self.limits.max_steps)
            })
            .map(|_| ())
            .map_err(|_used| LimitExceeded::Steps {
                maximum: self.limits.max_steps,
            })
    }

    pub(crate) fn retain(&self, bytes: u64) -> Result<RetainedBytes, LimitExceeded> {
        self.retained
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= self.limits.max_value_bytes)
            })
            .map_err(|_used| LimitExceeded::ValueBytes {
                maximum: self.limits.max_value_bytes,
            })?;
        Ok(RetainedBytes {
            used: Arc::clone(&self.retained),
            bytes,
            maximum: self.limits.max_value_bytes,
        })
    }

    pub(crate) fn steps(&self) -> u64 {
        self.steps.load(Ordering::Relaxed)
    }

    pub(crate) fn value_bytes(&self) -> u64 {
        self.retained.load(Ordering::Relaxed)
    }

    pub(crate) fn max_value_bytes(&self) -> u64 {
        self.limits.max_value_bytes
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
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    #[must_use]
    pub(crate) fn calls(&self) -> &CallBudget {
        &self.calls
    }
}
