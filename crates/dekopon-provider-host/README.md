# dekopon-provider-host

A bounded, synchronous Wasmtime component host for immediate-mode, read-only Dekopon providers.

Rust providers implement `dekopon_provider_sdk::Provider`; the SDK exports the world mirrored in [`wit/provider.wit`](wit/provider.wit). The host validates each provider manifest and typed response, rejects duplicate capability routes and non-read-only effects, uses one shared engine and runtime mutex to serialize calls, gives every call a fresh Wasmtime store and instance, and enforces memory, fuel, wall-clock, input, and output limits. A registry retains compiled components in memory but has no cross-process cache. Capability schemas must be object-shaped; operation-specific validation remains provider-owned. The linker supplies no WASI or other host imports, so components receive no filesystem, network, clock, environment, or credential authority.

This is the experimental local execution boundary, not the separately deployed authenticated broker path provided by `dekopon-brokerd`. It cannot authorize external effects and deliberately does not accept `AuthorizedInvocation`.
