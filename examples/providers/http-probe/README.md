# HTTP import probe

A minimal Rust provider that composes `dekopon:provider@0.1.0` with the `dekopon:http/client@1.0.0` import. It validates caller-generated provider worlds, the direct runner's fail-closed import boundary, and the broker component host's authorized path.

The single `http-probe.fetch` capability sends its required `uri` plus an optional arbitrary method token, ordered text headers, and buffered text body. Its test-only `catchError` input demonstrates that guest code cannot mask a policy rejection. Broker-host tests authorize only an ephemeral loopback mock server; they never contact the public internet. Direct `dekopon-run` loading fails during component instantiation because its linker remains empty.

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
