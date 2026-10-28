#![allow(clippy::disallowed_methods)]
use dekopon_broker_host::{
    BrokerHostLimits, BrokerProviderRegistry, enable_test_component_memoization,
};
use dekopon_test_support::provider_fixture;

#[tokio::test]
async fn loads_compile_each_time_before_opt_in() {
    for _ in 0..2 {
        BrokerProviderRegistry::load(
            [provider_fixture("cli-probe-provider.wasm")],
            BrokerHostLimits::default(),
        )
        .await
        .expect("fresh component load");
    }
    let handle = enable_test_component_memoization();
    assert_eq!(handle.bypass_compile_count(), 2);
    assert_eq!(handle.compile_count(), 0);
    assert_eq!(handle.hit_count(), 0);
}
