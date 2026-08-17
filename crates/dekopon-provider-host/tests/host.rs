use std::path::PathBuf;

use dekopon_provider_host::{HostLimits, PROVIDER_WIT, ProviderHostError, ProviderRegistry};
use serde_json::json;

fn provider_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("examples/providers/echo-provider.wasm")
}

fn imported_provider_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(format!("examples/providers/{name}"))
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
    assert_eq!(registry.capabilities().count(), 5);
}

#[test]
fn invokes_the_checked_in_text_transform_capabilities() {
    let registry = load_echo();
    let cases = [
        ("echo.reverse", "Hello 🦀", "🦀 olleH"),
        ("echo.upcase", "Hello, Straße!", "HELLO, STRASSE!"),
        ("echo.downcase", "Hello, WORLD!", "hello, world!"),
        ("echo.ransom-case", "Hello, World!", "hElLo, WoRlD!"),
    ];

    for (capability, input, expected) in cases {
        let capability = capability.parse().expect("valid capability fixture");
        let output = registry
            .invoke(&capability, &json!({"message": input}))
            .expect("text transformation succeeds");
        assert_eq!(output.output, json!({"message": expected}));
    }
}

#[test]
fn rejects_malformed_text_transform_inputs() {
    let registry = load_echo();
    let capability = "echo.upcase".parse().expect("valid capability fixture");

    let error = registry
        .invoke(&capability, &json!({"message": 42}))
        .expect_err("invalid message input must fail");

    assert!(matches!(
        error,
        ProviderHostError::ProviderFailure { ref code, .. } if code == "invalid-input"
    ));
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
fn immediate_host_rejects_components_requiring_http() {
    for fixture in [
        "http-probe-provider.wasm",
        "jsonplaceholder-provider.wasm",
        "gh-provider.wasm",
    ] {
        let error =
            ProviderRegistry::load([imported_provider_path(fixture)], HostLimits::default())
                .expect_err("the immediate linker must not satisfy HTTP imports");

        assert!(matches!(error, ProviderHostError::Instantiate { .. }));
    }
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
