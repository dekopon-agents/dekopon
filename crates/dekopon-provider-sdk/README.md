# dekopon-provider-sdk

Rust guest SDK for read-only Dekopon WebAssembly component providers.

Implement the `Provider` trait and call `export_provider!` once. The generated adapter exports the WIT world in [`wit/provider.wit`](wit/provider.wit), decodes JSON at the component boundary, and turns provider errors into a typed wire response. The host requires object-shaped inputs but does not generally enforce each capability's JSON Schema; provider implementations validate their own required fields, types, and constraints.

```rust,ignore
use dekopon_provider_sdk::{Provider, ProviderError, ProviderManifest};

struct Example;

impl Provider for Example {
    fn manifest() -> ProviderManifest { /* ... */ }
    fn invoke(/* ... */) -> Result<serde_json::Value, ProviderError> { /* ... */ }
}

dekopon_provider_sdk::export_provider!(Example);
```

The immediate host accepts only read-only manifests and supplies no WASI imports. The SDK WIT file is mirrored by `dekopon-provider-host`; update both copies together and keep their equality test passing.
