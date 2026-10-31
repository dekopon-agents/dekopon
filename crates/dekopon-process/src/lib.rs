#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used))]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        clippy::disallowed_types,
        reason = "tests spawn, join and drain freely; production sites carry their own expectation"
    )
)]
use std::time::Duration;
use tokio::sync::watch;
mod threads;
mod tree;
pub use threads::{ShellThreads, ThreadName, ThreadPermit};
pub use tree::{CallBudget, RetainedBytes, TreeContext};

pub const DEFAULT_MAX_STEPS: u64 = 100_000;
pub const DEFAULT_MAX_RECURSION_DEPTH: u32 = 32;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2_000;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_CAPABILITY_CALLS: u32 = 32;
pub const DEFAULT_MAX_VALUE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_steps: u64,
    pub max_recursion_depth: u32,
    pub max_output_bytes: usize,
    pub max_output_lines: usize,
    pub timeout: Duration,
    pub max_capability_calls: u32,
    pub max_value_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_output_lines: DEFAULT_MAX_OUTPUT_LINES,
            timeout: DEFAULT_TIMEOUT,
            max_capability_calls: DEFAULT_MAX_CAPABILITY_CALLS,
            max_value_bytes: DEFAULT_MAX_VALUE_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitExceeded {
    Steps { maximum: u64 },
    RecursionDepth { maximum: u32 },
    Deadline { timeout_ms: u128 },
    CapabilityCalls { maximum: u32 },
    ValueBytes { maximum: u64 },
    ShellThreads { maximum: u32 },
    Cancelled,
}

#[derive(Clone)]
pub struct CancelHandle {
    sender: watch::Sender<bool>,
}

impl CancelHandle {
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }
}

#[derive(Clone, Debug)]
pub struct CancelSignal {
    receiver: watch::Receiver<bool>,
}

impl CancelSignal {
    #[must_use]
    pub fn pair() -> (CancelHandle, Self) {
        let (sender, receiver) = watch::channel(false);
        (CancelHandle { sender }, Self { receiver })
    }

    #[must_use]
    pub fn never() -> Self {
        Self::pair().1
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    #[must_use]
    pub fn watch(&self) -> watch::Receiver<bool> {
        self.receiver.clone()
    }

    pub async fn cancelled(&self) {
        if self.watch().wait_for(|cancelled| *cancelled).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

mod exit;
mod job;
pub mod pipe;
pub use exit::ExitCode;
pub use job::{JOBS_OFF, JobId, JobOutcome, JobState, JobSummary, JobWait};

#[cfg(test)]
mod tests;
