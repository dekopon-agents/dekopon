//! Describe and command runs must be pure, so a refused clock read traps rather than errors, and
//! the store records the attempt instead of charging it.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::StoreState;
use crate::bindings::dekopon::clock::{monotonic, wall};

#[derive(Debug)]
pub(crate) enum ClockState {
    Granted {
        fixed: Option<SystemTime>,
        origin: Instant,
        failure: Option<&'static str>,
    },
    Refused {
        attempted: bool,
    },
}

impl ClockState {
    pub(crate) const fn describe() -> Self {
        Self::Refused { attempted: false }
    }

    pub(crate) fn invoke(fixed: Option<SystemTime>) -> Self {
        Self::Granted {
            fixed,
            origin: Instant::now(),
            failure: None,
        }
    }

    pub(crate) const fn failure(&self) -> Option<&'static str> {
        match self {
            Self::Granted { failure, .. } => *failure,
            Self::Refused { .. } => None,
        }
    }

    pub(crate) const fn attempted(&self) -> bool {
        matches!(self, Self::Refused { attempted: true })
    }
}

const CLOCK_REFUSED: &str =
    "provider read dekopon:clock/wall@1.1.0 outside invoke; describe and command runs are pure";
const MONOTONIC_REFUSED: &str = "provider read dekopon:clock/monotonic@1.1.0 outside invoke; describe and command runs are pure";

impl wall::Host for StoreState {
    async fn now_unix_millis(&mut self) -> wasmtime::Result<u64> {
        if let ClockState::Refused { attempted } = &mut self.clock {
            *attempted = true;
            return Err(wasmtime::Error::msg(CLOCK_REFUSED));
        }
        let now = match self.clock {
            ClockState::Granted { fixed, .. } => fixed.unwrap_or_else(SystemTime::now),
            ClockState::Refused { .. } => unreachable!("refused clock returned above"),
        };
        Ok(unix_millis(now))
    }
}

impl monotonic::Host for StoreState {
    async fn now_nanos(&mut self) -> wasmtime::Result<u64> {
        let (origin, failure) = match &mut self.clock {
            ClockState::Granted {
                origin, failure, ..
            } => (*origin, failure),
            ClockState::Refused { attempted } => {
                *attempted = true;
                return Err(wasmtime::Error::msg(MONOTONIC_REFUSED));
            }
        };
        let nanos = match u64::try_from(origin.elapsed().as_nanos()) {
            Ok(nanos) => nanos,
            Err(source) => {
                failure.get_or_insert("monotonic-overflow");
                tracing::error!(
                    event = "provider_monotonic_read",
                    status = "overflow",
                    ?source
                );
                return Err(
                    wasmtime::Error::new(source).context("monotonic invocation duration overflow")
                );
            }
        };
        Ok(nanos)
    }
}

fn unix_millis(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use wasmtime::Store;
    use wasmtime::component::{Component, Instance};

    use super::{ClockState, unix_millis};
    use crate::{
        BrokerHostLimits, BrokerHostOptions, Runtime, StoreState, http::HttpState,
        settings::SettingsState, storage::StorageState,
    };

    pub(crate) async fn component_in(
        runtime: &Runtime,
        clock: ClockState,
        wat: &str,
    ) -> (Store<StoreState>, Instance) {
        let component = Component::new(&runtime.engine, wat).expect("valid component fixture");
        let http = HttpState::describe(runtime.http_ceilings(), Duration::from_secs(5))
            .expect("disabled HTTP");
        let mut store = runtime
            .store_for_provider(
                "test-provider",
                http,
                StorageState::disabled(),
                clock,
                SettingsState::describe(),
            )
            .expect("bounded store");
        let instance = runtime
            .linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("fixture instantiates");
        (store, instance)
    }

    pub(crate) const SERVICES: &str = include_str!("../tests/fixtures/services.wat");

    #[tokio::test]
    async fn a_store_shares_monotonic_origin_across_components() {
        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("host runtime");
        let (mut store, first) = component_in(&runtime, ClockState::invoke(None), SERVICES).await;
        let component = Component::new(&runtime.engine, SERVICES).expect("fixture");
        let second = runtime
            .linker
            .instantiate_async(&mut store, &component)
            .await
            .expect("second component");
        let read_first = first
            .get_typed_func::<(), (u64,)>(&mut store, "read-clock")
            .expect("clock export");
        let read_second = second
            .get_typed_func::<(), (u64,)>(&mut store, "read-clock")
            .expect("clock export");
        let ClockState::Granted { origin, .. } = &store.data().clock else {
            panic!("invocation store has a monotonic origin");
        };
        let origin = *origin;
        let first_before = origin.elapsed().as_nanos();
        let start = read_first
            .call_async(&mut store, ())
            .await
            .expect("first reading")
            .0;
        let first_after = origin.elapsed().as_nanos();
        let second_before = origin.elapsed().as_nanos();
        let end = read_second
            .call_async(&mut store, ())
            .await
            .expect("second reading")
            .0;
        let second_after = origin.elapsed().as_nanos();
        assert!(first_before <= u128::from(start) && u128::from(start) <= first_after);
        assert!(second_before <= u128::from(end) && u128::from(end) <= second_after);
    }

    #[tokio::test]
    async fn a_provider_importing_wall_1_0_loads_through_the_broker_linker() {
        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("host runtime");
        let wat = r#"(component
            (import "dekopon:clock/wall@1.0.0" (instance $wall
                (export "now-unix-millis" (func (result u64)))))
            (core func $now (canon lower (func $wall "now-unix-millis")))
            (core module $m (import "host" "now" (func $now (result i64)))
                (func (export "read") (result i64) call $now))
            (core instance $i (instantiate $m (with "host" (instance (export "now" (func $now))))))
            (func (export "read") (result u64) (canon lift (core func $i "read"))))"#;
        let (mut store, instance) = component_in(&runtime, ClockState::invoke(None), wat).await;
        let read = instance
            .get_typed_func::<(), (u64,)>(&mut store, "read")
            .expect("wall export");
        assert!(read.call_async(&mut store, ()).await.expect("wall read").0 > 0);
    }

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
        assert!(!ClockState::invoke(None).attempted());
        assert!(!ClockState::describe().attempted());
        assert!(ClockState::Refused { attempted: true }.attempted());
    }
}
