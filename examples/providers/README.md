# Provider examples

- [`echo/`](echo/) is the Rust source for a provider implementing `dekopon_provider_sdk::Provider` with plain echo and deterministic reverse, uppercase, lowercase, and ransom-case capabilities.
- [`echo-provider.wasm`](echo-provider.wasm) is the generated component checked in so `dekopon-run` is usable without first installing a Wasm build toolchain.

Regenerate the component with the commands in [`echo/README.md`](echo/README.md). Do not edit the `.wasm` artifact directly.
