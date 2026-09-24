//! The policy is the sole terminal writer once a session starts, so a stopped reply can't land
//! ahead of the partial answer it follows; the text type itself guarantees no model text or
//! credential reaches a progress line.

use std::time::Duration;

use dekopon_agent::{BudgetLimit, CancelSource, CancelVia};

mod adapter;
mod policy;
mod text;

#[cfg(test)]
mod tests;

pub(crate) use policy::{ProgressInputs, ProgressPolicy, Terminal};
pub(crate) use text::{ProgressDetail, ProgressText, Templates};

pub(crate) const DEFAULT_KEEP_ALIVE_AT: [u64; 2] = [15, 45];
pub(crate) const DEFAULT_KEEP_ALIVE_EVERY: u64 = 60;
pub(crate) const DEFAULT_KEEP_ALIVE_MAX: u32 = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeepAlive {
    pub at: Vec<Duration>,
    pub every: Duration,
    pub max: u32,
}

impl Default for KeepAlive {
    fn default() -> Self {
        Self {
            at: DEFAULT_KEEP_ALIVE_AT
                .iter()
                .map(|seconds| Duration::from_secs(*seconds))
                .collect(),
            every: Duration::from_secs(DEFAULT_KEEP_ALIVE_EVERY),
            max: DEFAULT_KEEP_ALIVE_MAX,
        }
    }
}

pub(crate) const fn cancel_label(source: CancelSource) -> &'static str {
    match source {
        CancelSource::User {
            via: CancelVia::NativeStop,
        } => "user:native-stop",
        CancelSource::User {
            via: CancelVia::Button,
        } => "user:button",
        CancelSource::User {
            via: CancelVia::StopReply,
        } => "user:stop-reply",
        CancelSource::Operator => "operator",
        CancelSource::Budget {
            limit: BudgetLimit::WallClock,
        } => "budget:wall-clock",
    }
}
