# dekopon-provider-host

A bounded, synchronous Wasmtime component host for immediate-mode, read-only Dekopon providers.

Rust providers implement `dekopon_provider_sdk::Provider`; the SDK exports the world mirrored in [`wit/provider.wit`](wit/provider.wit). The host validates each provider manifest and typed response, rejects duplicate capability routes and non-read-only effects, serializes calls through one shared engine, gives every call a fresh Wasmtime store, and enforces memory, fuel, wall-clock, input, and output limits. The linker supplies no WASI or other host imports, so components receive no filesystem, network, clock, environment, or credential authority.

This is an experimental local execution boundary, not the planned authenticated broker. It cannot authorize external effects and deliberately does not accept `AuthorizedInvocation`.
