# HTTP import probe

A minimal Rust provider that composes `dekopon:provider@0.2.0` with the `dekopon:http/client@1.0.0` import. It validates caller-generated provider worlds, the direct runner's fail-closed import boundary, and the broker component host's authorized path.

`http-probe.fetch` sends its required `uri` plus an optional arbitrary method token, ordered text headers, and buffered text body. Its test-only `catchError` input demonstrates that guest code cannot mask a policy rejection.

`http-probe.conditional-write` is the two-call capability: it reads the resource, then writes only if the etag it observed is still current, refusing in between. It exists so the broker host has an in-tree capability that makes *two* authorized calls in one invocation, which is what exercises `maxRequests`, per-call evidence, and the host-call limit — the shape `gh.pull-request.approve` used to cover before the GitHub provider moved to its own repository. `http-probe.purge` deletes one resource, and exists so the manifest exposes something [`../../conditional-write/`](../../conditional-write/README.md) deliberately grants nowhere. Broker-host tests authorize only an ephemeral loopback mock server; they never contact the public internet. Direct `dekopon-run` loading fails during component instantiation because its linker remains empty.

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
