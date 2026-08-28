# dekopon-core

Validated identifiers and dependency-light domain types shared by Dekopon crates.

It also holds the small pure helpers that separate listeners must not disagree about: the `accept()`
retry classification with its backoff bounds, and `error_chain`, which renders a failure and its
sources as one line.

This crate contains no transport, CLI, async runtime, policy-engine, or provider-host dependencies.
The `errno` table brings `libc` on Unix targets only, so a wasm guest build pulls none of it.
