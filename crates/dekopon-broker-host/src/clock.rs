//! The `dekopon:clock/wall@1.0.0` import: the host's wall clock, readable during `invoke` only.
//!
//! `describe`, `run-command`, and `resolve-command` are pure by contract. HTTP and storage enforce
//! that with disabled states whose calls return a typed denial and are refused afterwards through
//! the `DescribeUsedHostImport` and `RunCommandUsedHostImport` tripwires. `now-unix-millis` has no
//! error in its signature, so a read outside an invocation traps instead, and the store remembers
//! the attempt so the same tripwires name it rather than the trap. Nothing is charged: a read has
//! no effect and allocates nothing, and the guest loop around it is already bounded by fuel and the
//! operation deadline.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::StoreState;
use crate::bindings::dekopon::clock::wall;

/// Whether a store's guest may read the wall clock, and whether a refused read was attempted.
#[derive(Debug)]
pub(crate) enum ClockState {
    /// An authorized invocation.
    Granted,
    /// A description or command run: the contract is pure, so a read traps.
    Refused {
        /// Whether the guest reached for the clock anyway.
        attempted: bool,
    },
}

impl ClockState {
    /// The clock for a description or a command run.
    pub(crate) const fn describe() -> Self {
        Self::Refused { attempted: false }
    }

    /// The clock for an authorized invocation.
    pub(crate) const fn invoke() -> Self {
        Self::Granted
    }

    /// Whether the guest read, or tried to read, a clock it was refused.
    pub(crate) const fn attempted(&self) -> bool {
        matches!(self, Self::Refused { attempted: true })
    }
}

/// The trap a refused read raises.
const CLOCK_REFUSED: &str =
    "provider read dekopon:clock/wall@1.0.0 outside invoke; describe and command runs are pure";

impl wall::Host for StoreState {
    async fn now_unix_millis(&mut self) -> wasmtime::Result<u64> {
        if let ClockState::Refused { attempted } = &mut self.clock {
            *attempted = true;
            return Err(wasmtime::Error::msg(CLOCK_REFUSED));
        }
        let unix_millis = unix_millis(SystemTime::now());
        // Goal 2: a value the host handed the guest is part of the run, so the trace records it,
        // inside the `provider.invoke` span this store runs under.
        tracing::info!(event = "provider_clock_read", unix_millis);
        Ok(unix_millis)
    }
}

/// Milliseconds since the Unix epoch, saturating at both ends.
///
/// The only failure `duration_since` has is a host clock set before 1970, which reads as `0`; that
/// reading is what `provider_clock_read` records, so the cause stays visible.
fn unix_millis(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{ClockState, unix_millis};

    #[test]
    fn unix_millis_truncates_and_saturates_at_the_epoch() {
        assert_eq!(unix_millis(UNIX_EPOCH), 0);
        assert_eq!(
            unix_millis(UNIX_EPOCH + Duration::from_micros(951_782_400_000_999)),
            951_782_400_000
        );
        let before_epoch = UNIX_EPOCH
            .checked_sub(Duration::from_secs(1))
            .expect("the platform represents 1969");
        assert_eq!(unix_millis(before_epoch), 0);
        assert!(unix_millis(SystemTime::now()) > 951_782_400_000);
    }

    #[test]
    fn only_a_refused_read_counts_as_attempted() {
        assert!(!ClockState::invoke().attempted());
        assert!(!ClockState::describe().attempted());
        assert!(ClockState::Refused { attempted: true }.attempted());
    }
}
