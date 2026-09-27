//! Describe and command runs must be pure, so a refused clock read traps rather than errors, and
//! the store records the attempt instead of charging it.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::StoreState;
use crate::bindings::dekopon::{
    clock1_0_0::wall,
    clock1_1_0::{monotonic, wall as wall_v1_1},
};

#[derive(Debug)]
pub(crate) enum ClockState {
    Granted { origin: Instant },
    Refused { attempted: bool },
}

impl ClockState {
    pub(crate) const fn describe() -> Self {
        Self::Refused { attempted: false }
    }

    pub(crate) fn invoke() -> Self {
        Self::Granted {
            origin: Instant::now(),
        }
    }

    pub(crate) const fn attempted(&self) -> bool {
        matches!(self, Self::Refused { attempted: true })
    }
}

const CLOCK_REFUSED: &str =
    "provider read dekopon:clock/wall@1.0.0 outside invoke; describe and command runs are pure";
const CLOCK_V1_1_REFUSED: &str =
    "provider read dekopon:clock/wall@1.1.0 outside invoke; describe and command runs are pure";

impl StoreState {
    fn wall_millis(&mut self, refusal: &'static str) -> wasmtime::Result<u64> {
        self.clock.refuse(refusal)?;
        let unix_millis = unix_millis(SystemTime::now());
        tracing::info!(event = "provider_clock_read", unix_millis);
        Ok(unix_millis)
    }
}

impl wall::Host for StoreState {
    async fn now_unix_millis(&mut self) -> wasmtime::Result<u64> {
        self.wall_millis(CLOCK_REFUSED)
    }
}

impl wall_v1_1::Host for StoreState {
    async fn now_unix_millis(&mut self) -> wasmtime::Result<u64> {
        self.wall_millis(CLOCK_V1_1_REFUSED)
    }
}

const MONOTONIC_REFUSED: &str = "provider read dekopon:clock/monotonic@1.1.0 outside invoke";

impl monotonic::Host for StoreState {
    async fn now_nanos(&mut self) -> wasmtime::Result<u64> {
        let origin = self.clock.refuse(MONOTONIC_REFUSED)?;
        let nanos = elapsed_nanos(origin, Instant::now()).map_err(|source| {
            wasmtime::Error::new(source).context("monotonic invocation duration overflow")
        })?;
        tracing::info!(event = "provider_monotonic_read", nanos);
        Ok(nanos)
    }
}

impl ClockState {
    fn refuse(&mut self, message: &'static str) -> wasmtime::Result<Instant> {
        match self {
            Self::Granted { origin } => Ok(*origin),
            Self::Refused { attempted } => {
                *attempted = true;
                Err(wasmtime::Error::msg(message))
            }
        }
    }
}

fn elapsed_nanos(origin: Instant, now: Instant) -> Result<u64, std::num::TryFromIntError> {
    u64::try_from(now.duration_since(origin).as_nanos())
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
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::{ClockState, elapsed_nanos, unix_millis};

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
    fn elapsed_nanoseconds_use_the_supplied_invocation_origin() {
        let first = Instant::now();
        let later = first + Duration::from_secs(3);
        let reset = later + Duration::from_secs(1);
        assert_eq!(elapsed_nanos(first, later).expect("fits"), 3_000_000_000);
        assert_eq!(elapsed_nanos(reset, reset).expect("fits"), 0);
    }

    #[test]
    fn only_a_refused_read_counts_as_attempted() {
        assert!(!ClockState::invoke().attempted());
        assert!(!ClockState::describe().attempted());
        assert!(ClockState::Refused { attempted: true }.attempted());
    }
}
