//! Describe and command runs must be pure, so a refused clock read traps rather than errors, and
//! the store records the attempt instead of charging it.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::StoreState;
use crate::bindings::dekopon::clock::wall;

#[derive(Debug)]
pub(crate) enum ClockState {
    Granted,
    Refused { attempted: bool },
}

impl ClockState {
    pub(crate) const fn describe() -> Self {
        Self::Refused { attempted: false }
    }

    pub(crate) const fn invoke() -> Self {
        Self::Granted
    }

    pub(crate) const fn attempted(&self) -> bool {
        matches!(self, Self::Refused { attempted: true })
    }
}

const CLOCK_REFUSED: &str =
    "provider read dekopon:clock/wall@1.0.0 outside invoke; describe and command runs are pure";

impl wall::Host for StoreState {
    async fn now_unix_millis(&mut self) -> wasmtime::Result<u64> {
        if let ClockState::Refused { attempted } = &mut self.clock {
            *attempted = true;
            return Err(wasmtime::Error::msg(CLOCK_REFUSED));
        }
        let unix_millis = unix_millis(SystemTime::now());
        tracing::info!(event = "provider_clock_read", unix_millis);
        Ok(unix_millis)
    }
}

/// Saturates to zero only when the host clock predates 1970; that same reading is what gets logged,
/// so the cause stays visible.
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
