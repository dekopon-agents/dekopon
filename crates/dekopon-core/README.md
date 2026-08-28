# dekopon-core

Validated identifiers and dependency-light domain types shared by Dekopon crates.

It also holds the small helpers that separate processes must not disagree about: the `accept()`
retry classification with its backoff bounds, `error_chain`, which renders a failure and its sources
as one line, and `read_trusted_file`, the one definition of what makes a local file trusted input —
opened without following a symlink, regular, single-link, owned by this process, within a byte
ceiling, and at one of two named permission tiers.

This crate contains no transport, CLI, async runtime, policy-engine, or provider-host dependencies.
The `errno` table and the trusted-file predicate bring `libc` on Unix targets only, so a wasm guest
build pulls none of it; the predicate itself is Unix-only, and its callers wrap the blocking read in
one `spawn_blocking` rather than importing an async runtime here.
