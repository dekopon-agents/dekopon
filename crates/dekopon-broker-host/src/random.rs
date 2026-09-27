use crate::{StoreState, bindings::dekopon::random::source};

pub(crate) const MAX_RANDOM_BYTES: u32 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RandomFailure {
    TooLarge,
    Entropy,
}

pub(crate) enum RandomState {
    Refused { attempted: bool },
    Granted { failure: Option<RandomFailure> },
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

    pub(crate) const fn failure(&self) -> Option<RandomFailure> {
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
            failure.get_or_insert(RandomFailure::TooLarge);
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
            failure.get_or_insert(RandomFailure::Entropy);
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
    use super::{MAX_RANDOM_BYTES, RandomFailure, RandomState};
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
        random: RandomState,
        wat: &str,
    ) -> (Store<StoreState>, Instance) {
        let component = Component::new(&runtime.engine, wat).expect("valid component fixture");
        let http = HttpState::describe(runtime.http_ceilings(), Duration::from_secs(5))
            .expect("disabled HTTP context");
        let mut store = runtime
            .store(
                http,
                StorageState::disabled(),
                clock,
                random,
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
    async fn wasmtime_services_refuse_metadata_and_oversize_calls() {
        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("host runtime");
        let (mut store, instance) = component_in(
            &runtime,
            ClockState::describe(),
            RandomState::describe(),
            SERVICES,
        )
        .await;
        let read_clock = instance
            .get_typed_func::<(), (u64,)>(&mut store, "read-clock")
            .expect("clock export");
        assert!(read_clock.call_async(&mut store, ()).await.is_err());
        assert!(store.data().clock.attempted());
        let (mut store, instance) = component_in(
            &runtime,
            ClockState::describe(),
            RandomState::describe(),
            SERVICES,
        )
        .await;
        let read_random = instance
            .get_typed_func::<(u32,), (u32,)>(&mut store, "read-random")
            .expect("random export");
        assert!(read_random.call_async(&mut store, (0,)).await.is_err());
        assert!(store.data().random.attempted());

        let (mut store, instance) = component_in(
            &runtime,
            ClockState::invoke(),
            RandomState::invoke(),
            SERVICES,
        )
        .await;
        let read_random = instance
            .get_typed_func::<(u32,), (u32,)>(&mut store, "read-random")
            .expect("random export");
        assert_eq!(
            read_random
                .call_async(&mut store, (0,))
                .await
                .expect("empty read")
                .0,
            0
        );
        assert_eq!(
            read_random
                .call_async(&mut store, (32,))
                .await
                .expect("OS entropy read")
                .0,
            32
        );
        assert!(
            read_random
                .call_async(&mut store, (MAX_RANDOM_BYTES + 1,))
                .await
                .is_err()
        );
        assert_eq!(store.data().random.failure(), Some(RandomFailure::TooLarge));
    }

    #[tokio::test]
    async fn invocation_owned_instantiation_has_services_but_metadata_instantiation_does_not() {
        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("host runtime");
        for (startup, clock_attempted) in [
            ("(func $init call $clock drop) (start $init)", true),
            (
                "(func $init i32.const 0 i32.const 16 call $random) (start $init)",
                false,
            ),
        ] {
            let wat = SERVICES.replace(
                "    (func (export \"read-clock\")",
                &format!("    {startup}\n    (func (export \"read-clock\")"),
            );
            let component = Component::new(&runtime.engine, &wat).expect("startup component");
            let http =
                HttpState::describe(runtime.http_ceilings(), Duration::from_secs(5)).expect("HTTP");
            let mut store = runtime
                .store(
                    http,
                    StorageState::disabled(),
                    ClockState::describe(),
                    RandomState::describe(),
                    SettingsState::describe(),
                )
                .expect("store");
            assert!(
                runtime
                    .linker
                    .instantiate_async(&mut store, &component)
                    .await
                    .is_err()
            );
            assert_eq!(store.data().clock.attempted(), clock_attempted);
            assert_eq!(store.data().random.attempted(), !clock_attempted);
            let (_store, _instance) =
                component_in(&runtime, ClockState::invoke(), RandomState::invoke(), &wat).await;
        }
    }

    #[tokio::test]
    async fn wasmtime_monotonic_reads_share_the_invocation_origin() {
        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("host runtime");
        let (mut store, instance) = component_in(
            &runtime,
            ClockState::invoke(),
            RandomState::invoke(),
            SERVICES,
        )
        .await;
        let read_clock = instance
            .get_typed_func::<(), (u64,)>(&mut store, "read-clock")
            .expect("clock export");
        let first = read_clock
            .call_async(&mut store, ())
            .await
            .expect("first elapsed read")
            .0;
        let second = read_clock
            .call_async(&mut store, ())
            .await
            .expect("second elapsed read")
            .0;
        assert!(second >= first);
        let origin = match &store.data().clock {
            ClockState::Granted { origin } => *origin,
            ClockState::Refused { .. } => panic!("invoke has an origin"),
        };
        assert!(u128::from(second) <= origin.elapsed().as_nanos());
    }

    #[test]
    fn guest_and_host_agree_on_the_per_call_ceiling() {
        assert_eq!(MAX_RANDOM_BYTES, dekopon_provider_random::MAX_RANDOM_BYTES);
    }

    #[test]
    fn size_and_phase_refusals_never_consult_entropy() {
        let mut refused = RandomState::describe();
        assert!(
            refused
                .read(0, |_| panic!("no entropy in describe"))
                .is_err()
        );
        assert!(refused.attempted());
        let mut granted = RandomState::invoke();
        assert!(
            granted
                .read(MAX_RANDOM_BYTES + 1, |_| panic!("oversize before source"))
                .is_err()
        );
        assert_eq!(granted.failure(), Some(RandomFailure::TooLarge));
    }

    #[test]
    fn zero_length_and_exact_bytes_are_forwarded_without_a_fallback() {
        let mut granted = RandomState::invoke();
        assert!(
            granted
                .read(0, |_| panic!("zero does not need an OS call"))
                .expect("zero permitted")
                .is_empty()
        );
        assert_eq!(
            granted
                .read(4, |bytes| {
                    bytes.copy_from_slice(&[1, 2, 3, 4]);
                    Ok(())
                })
                .expect("fixed test source"),
            [1, 2, 3, 4]
        );
        assert_eq!(granted.failure(), None);
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
                .expect("test entropy");
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

    #[test]
    fn source_failure_is_terminal_and_returns_no_partial_bytes() {
        let mut granted = RandomState::invoke();
        assert!(
            granted
                .read(2, |bytes| {
                    bytes[0] = 42;
                    Err(getrandom::Error::UNEXPECTED)
                })
                .is_err()
        );
        assert_eq!(granted.failure(), Some(RandomFailure::Entropy));
    }
}
