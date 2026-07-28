use std::path::PathBuf;

use dekopon_provider_host::{HostLimits, PROVIDER_WIT, ProviderHostError, ProviderRegistry};
use serde_json::json;

fn provider_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/providers/echo-provider.wasm")
}

fn load_echo() -> ProviderRegistry {
    ProviderRegistry::load([provider_path()], HostLimits::default())
        .expect("Rust echo provider loads")
}

#[test]
fn loads_routes_and_invokes_the_rust_provider_component() {
    let registry = load_echo();
    let capability = "echo.echo".parse().expect("valid capability fixture");
    let input = json!({"message": "hello"});

    let first = registry
        .invoke(&capability, &input)
        .expect("first invocation succeeds");
    let second = registry
        .invoke(&capability, &json!({"message": "again"}))
        .expect("second invocation succeeds");

    assert_eq!(first.provider.as_str(), "echo");
    assert_eq!(first.output, input);
    assert_eq!(second.output, json!({"message": "again"}));
    assert_eq!(registry.manifests().len(), 1);
    assert_eq!(registry.capabilities().count(), 1);
}

#[test]
fn rejects_duplicate_provider_components() {
    let path = provider_path();
    let error = ProviderRegistry::load([path.clone(), path], HostLimits::default())
        .expect_err("duplicate providers must fail");

    assert!(matches!(error, ProviderHostError::DuplicateProvider { .. }));
}

#[test]
fn enforces_serialized_input_bounds_before_execution() {
    let limits = HostLimits {
        max_input_bytes: 8,
        ..HostLimits::default()
    };
    let registry =
        ProviderRegistry::load([provider_path()], limits).expect("Rust echo provider loads");
    let capability = "echo.echo".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!({"message": "too large"}))
        .expect_err("oversized input must fail");

    assert!(matches!(error, ProviderHostError::InputTooLarge { .. }));
}

#[test]
fn rejects_non_object_inputs_before_entering_wasm() {
    let registry = load_echo();
    let capability = "echo.echo".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!("not an object"))
        .expect_err("provider inputs must be objects");

    assert!(matches!(error, ProviderHostError::InputNotObject { .. }));
}

#[test]
fn reports_unknown_capabilities_without_entering_wasm() {
    let registry = load_echo();
    let capability = "missing.read".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!({}))
        .expect_err("unknown capability must fail");

    assert!(matches!(error, ProviderHostError::UnknownCapability { .. }));
}

#[test]
fn host_and_guest_sdk_use_the_same_wit_contract() {
    assert_eq!(PROVIDER_WIT, dekopon_provider_sdk::PROVIDER_WIT);
}

#[test]
fn rejects_zero_execution_limits_before_compiling_components() {
    let limits = HostLimits {
        fuel: 0,
        ..HostLimits::default()
    };
    let error =
        ProviderRegistry::load([provider_path()], limits).expect_err("zero limits must fail");

    assert!(matches!(
        error,
        ProviderHostError::InvalidLimit { name: "fuel" }
    ));
}
