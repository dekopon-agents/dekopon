# HTTP import probe

A minimal Rust provider that composes `dekopon:provider@0.1.0` with the `dekopon:http/client@1.0.0` import. It exists to validate caller-generated provider worlds and the direct runner's fail-closed import boundary before the broker host lands.

The single `http-probe.fetch` capability would issue `GET https://example.invalid/`. That reserved endpoint is intentionally non-routable and is never contacted by repository tests. Direct `dekopon-run` loading fails during component instantiation because its linker remains empty.

Run native checks:

```console
cargo fmt --manifest-path examples/providers/http-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/http-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/http-probe/Cargo.toml
```

Build and inspect the generated component:

```console
examples/providers/http-probe/build.sh
wasm-tools validate examples/providers/http-probe-provider.wasm
wasm-tools component wit examples/providers/http-probe-provider.wasm
```

The decoded component must export `describe` and `invoke`, import exactly `dekopon:http/client@1.0.0`, and import no WASI interfaces.
