# Provider examples

- [`echo/`](echo/) is the Rust source for a provider implementing `dekopon_provider_sdk::Provider` with plain echo and deterministic reverse, uppercase, lowercase, and ransom-case capabilities.
- [`echo-provider.wasm`](echo-provider.wasm) is the generated component checked in so `dekopon-run` is usable without first installing a Wasm build toolchain.
- [`http-probe/`](http-probe/) composes the provider exports with `dekopon:http/client@1.0.0` and validates the generalized SDK world adapter plus the broker component host.
- [`http-probe-provider.wasm`](http-probe-provider.wasm) is its generated component fixture. The direct runner intentionally rejects it because the immediate linker remains empty; broker-host tests execute it only against ephemeral loopback servers under exact constraints.
- [`jsonplaceholder/`](jsonplaceholder/) implements separately named post-read and external-write operations with bounded typed inputs and production-origin or literal-loopback endpoint validation.
- [`jsonplaceholder-provider.wasm`](jsonplaceholder-provider.wasm) is its generated component. Native and broker tests use injected or ephemeral loopback mocks and never contact the public JSONPlaceholder service.

Regenerate each component with the commands in its source README. Do not edit `.wasm` artifacts directly.
