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
mod tree;
pub use tree::{CallBudget, RetainedBytes, TreeContext};

pub const DEFAULT_MAX_STEPS: u64 = 100_000;
/// Function calls, `$( )`, `${ }` and compound commands all recurse on the native stack against
/// this one depth; 32 keeps the deepest measured script inside a 2 MiB thread even in a debug build.
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
        let (sender, receiver) = watch::channel(false);
        drop(sender);
        Self { receiver }
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
        let mut receiver = self.watch();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(test)]
mod tests;
