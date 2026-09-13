//! Everything a person waiting on one message is shown, and who is allowed to write it.
//!
//! Three pieces, each with one job. [`ProgressAdapter`](adapter::ProgressAdapter) is the sink the
//! synchronous prompt loop emits into: it writes one `gateway.progress` record per event and hands
//! the event on without blocking. [`ProgressPolicy`] is the per-session task that decides *when*
//! something changes on screen, spends the session's edit budget, and owns the one message the
//! gateway may edit. The `text` half owns what that message can say, and its type makes "no model
//! text and no credential reaches a progress line" a property of the code rather than a rule to
//! remember.
//!
//! The policy is the only terminal writer once a session has started. Nothing else replies for a
//! cancelled or failed session, which is what keeps `Stopped.` from landing ahead of the partial
//! answer it is supposed to follow.

use std::time::Duration;

use dekopon_agent::{BudgetLimit, CancelSource, CancelVia};

mod adapter;
mod policy;
mod text;

#[cfg(test)]
mod tests;

pub(crate) use policy::{ProgressInputs, ProgressPolicy, Terminal};
pub(crate) use text::{ProgressDetail, ProgressText, Templates};

/// Default offsets from `Started` at which the first keep-alive ticks fire.
pub(crate) const DEFAULT_KEEP_ALIVE_AT: [u64; 2] = [15, 45];
/// Default period between keep-alive ticks after the listed offsets.
pub(crate) const DEFAULT_KEEP_ALIVE_EVERY: u64 = 60;
/// Default ceiling on keep-alive ticks in one session.
pub(crate) const DEFAULT_KEEP_ALIVE_MAX: u32 = 10;

/// When a session says it is still alive, after validation.
///
/// Two shapes in one because a person reads the first minute differently from the tenth: the
/// listed offsets cover "did it hear me at all", and the period after them covers "is it still
/// going". `max` is the bound, because a run that never ends must not write forever.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeepAlive {
    /// Offsets from `Started`, in order.
    pub at: Vec<Duration>,
    /// Period between ticks once the listed offsets are spent.
    pub every: Duration,
    /// Ticks one session may write.
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

/// Stable low-cardinality label for one cancellation origin.
///
/// A label rather than a `Debug` rendering because this reaches telemetry, where a shape that
/// changes with a field name is a dashboard that breaks silently.
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
