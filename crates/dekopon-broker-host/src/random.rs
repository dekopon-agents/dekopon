use crate::{StoreState, bindings::dekopon::random::source};

const MAX_RANDOM_BYTES: u32 = 4096;

pub(crate) enum RandomState {
    Refused { attempted: bool },
    Granted { failure: Option<&'static str> },
}

impl RandomState {
    pub(crate) const fn describe() -> Self {
        Self::Refused { attempted: false }
    }

    pub(crate) const fn invoke() -> Self {
        Self::Granted { failure: None }
    }

    pub(crate) const fn attempted(&self) -> bool {
        matches!(self, Self::Refused { attempted: true })
    }

    pub(crate) const fn failure(&self) -> Option<&'static str> {
        match self {
            Self::Granted { failure } => *failure,
            Self::Refused { .. } => None,
        }
    }

    fn read(
        &mut self,
        length: u32,
        fill: impl FnOnce(&mut [u8]) -> Result<(), getrandom::Error>,
    ) -> wasmtime::Result<Vec<u8>> {
        let failure = match self {
            Self::Refused { attempted } => {
                *attempted = true;
                tracing::info!(
                    event = "provider_random_read",
                    length,
                    status = "refused-phase"
                );
                return Err(wasmtime::Error::msg("random bytes outside invoke"));
            }
            Self::Granted { failure } => failure,
        };
        if length > MAX_RANDOM_BYTES {
            failure.get_or_insert("random-too-large");
            tracing::info!(
                event = "provider_random_read",
                length,
                status = "refused-size"
            );
            return Err(wasmtime::Error::msg("random request exceeds host limit"));
        }
        let mut bytes = vec![0; length as usize];
        if length == 0 {
            tracing::info!(event = "provider_random_read", length, status = "succeeded");
            return Ok(bytes);
        }
        if let Err(source) = fill(&mut bytes) {
            failure.get_or_insert("random-entropy");
            tracing::error!(
                event = "provider_random_read",
                length,
                status = "entropy-failed",
                ?source
            );
            return Err(wasmtime::Error::msg("OS entropy unavailable"));
        }
        tracing::info!(event = "provider_random_read", length, status = "succeeded");
        Ok(bytes)
    }
}

impl source::Host for StoreState {
    async fn get_random_bytes(&mut self, length: u32) -> wasmtime::Result<Vec<u8>> {
        self.random.read(length, getrandom::fill)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_RANDOM_BYTES, RandomState};
    use crate::{
        BrokerHostLimits, BrokerHostOptions, ClockState, Runtime, StoreState, http::HttpState,
        settings::SettingsState, storage::StorageState,
    };
    use std::time::Duration;
    use wasmtime::Store;
    use wasmtime::component::{Component, Instance};

    async fn component_in(
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

    const SERVICES: &str = include_str!("../tests/fixtures/services.wat");

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

    #[tokio::test]
    async fn real_component_reads_os_entropy() {
        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("host runtime");
        let (mut store, instance) =
            component_in(&runtime, ClockState::invoke(None), SERVICES).await;
        let read = instance
            .get_typed_func::<(u32,), (u32,)>(&mut store, "read-random")
            .expect("random export");
        assert_eq!(
            read.call_async(&mut store, (32,))
                .await
                .expect("OS entropy")
                .0,
            32
        );
    }

    #[test]
    fn phase_is_checked_before_entropy() {
        let mut refused = RandomState::describe();
        assert!(
            refused
                .read(0, |_| panic!("entropy outside invoke"))
                .is_err()
        );
        assert!(refused.attempted());
    }

    #[test]
    fn oversize_is_refused_before_allocation_or_entropy() {
        let mut granted = RandomState::invoke();
        assert!(
            granted
                .read(MAX_RANDOM_BYTES + 1, |_| panic!("entropy for oversize"))
                .is_err()
        );
        assert_eq!(granted.failure(), Some("random-too-large"));
    }

    #[test]
    fn empty_request_never_consults_entropy() {
        let mut granted = RandomState::invoke();
        assert!(
            granted
                .read(0, |_| panic!("entropy for empty request"))
                .expect("empty read")
                .is_empty()
        );
        assert_eq!(granted.failure(), None);
    }

    #[test]
    fn entropy_failure_returns_no_partial_bytes_and_records_refusal() {
        let mut granted = RandomState::invoke();
        assert!(
            granted
                .read(2, |bytes| {
                    bytes[0] = 42;
                    Err(getrandom::Error::UNEXPECTED)
                })
                .is_err()
        );
        assert_eq!(granted.failure(), Some("random-entropy"));
    }

    #[test]
    fn entropy_trace_records_length_and_status_without_bytes() {
        use dekopon_test_support::CaptureLayer;
        use tracing_subscriber::layer::SubscriberExt as _;

        let capture = CaptureLayer::workspace();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, || {
            let bytes = RandomState::invoke()
                .read(4, |out| {
                    out.copy_from_slice(&[111, 222, 111, 222]);
                    Ok(())
                })
                .expect("entropy read");
            assert_eq!(bytes, [111, 222, 111, 222]);
        });
        let recorded = capture.events();
        assert_eq!(recorded.len(), 1);
        let fields = &recorded[0].0;
        assert!(fields.contains("provider_random_read"));
        assert!(fields.contains("length=4"));
        assert!(fields.contains("status=\"succeeded\""));
        assert!(!fields.contains("111"));
        assert!(!fields.contains("222"));
    }
}
