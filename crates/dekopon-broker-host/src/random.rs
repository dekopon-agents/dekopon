use crate::{ClockState, StoreState, bindings::dekopon::random::source};

const MAX_RANDOM_BYTES: u32 = 4096;

fn read(
    clock: &mut ClockState,
    length: u32,
    fill: impl FnOnce(&mut [u8]) -> Result<(), getrandom::Error>,
) -> wasmtime::Result<Vec<u8>> {
    let failure = match clock {
        ClockState::Refused { attempted } => {
            *attempted = true;
            tracing::info!(
                event = "provider_random_read",
                length,
                status = "refused-phase"
            );
            return Err(wasmtime::Error::msg("random bytes outside invoke"));
        }
        ClockState::Granted { failure, .. } => failure,
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

impl source::Host for StoreState {
    async fn get_random_bytes(&mut self, length: u32) -> wasmtime::Result<Vec<u8>> {
        read(&mut self.clock, length, getrandom::fill)
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_RANDOM_BYTES, read};
    use crate::{
        BrokerHostLimits, BrokerHostOptions, ClockState, Runtime,
        clock::tests::{SERVICES, component_in},
    };

    #[tokio::test]
    async fn real_component_reads_os_entropy() {
        let runtime = Runtime::new(BrokerHostLimits::default(), &BrokerHostOptions::default())
            .expect("host runtime");
        let (mut store, instance) =
            component_in(&runtime, ClockState::invoke(None), SERVICES).await;
        let read_random = instance
            .get_typed_func::<(u32,), (u32,)>(&mut store, "read-random")
            .expect("random export");
        assert_eq!(
            read_random
                .call_async(&mut store, (32,))
                .await
                .expect("OS entropy")
                .0,
            32
        );
    }

    #[test]
    fn phase_is_checked_before_entropy() {
        let mut refused = ClockState::describe();
        assert!(read(&mut refused, 0, |_| panic!("entropy outside invoke")).is_err());
        assert!(refused.attempted());
    }

    #[test]
    fn oversize_is_refused_before_allocation_or_entropy() {
        let mut granted = ClockState::invoke(None);
        assert!(
            read(&mut granted, MAX_RANDOM_BYTES + 1, |_| panic!(
                "entropy for oversize"
            ))
            .is_err()
        );
        assert_eq!(granted.failure(), Some("random-too-large"));
    }

    #[test]
    fn empty_request_never_consults_entropy() {
        let mut granted = ClockState::invoke(None);
        assert!(
            read(&mut granted, 0, |_| panic!("entropy for empty request"))
                .expect("empty read")
                .is_empty()
        );
        assert_eq!(granted.failure(), None);
    }

    #[test]
    fn entropy_failure_returns_no_partial_bytes_and_records_refusal() {
        let mut granted = ClockState::invoke(None);
        assert!(
            read(&mut granted, 2, |bytes| {
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
            let bytes = read(&mut ClockState::invoke(None), 4, |out| {
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
