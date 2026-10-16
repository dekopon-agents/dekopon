# Synthetic context replay

This experimental replay takes already eligible, scripted records. Each cassette request must match the record, ordinal, version, and config identity exactly. The response names the representation to render. The portable Rust crate renders that representation and fits whole visible turns; retained records remain visible separately in each snapshot. No inference request, network call, journal read, authority decision, or model request is produced.

Build and run from the repository root with the pinned Rust toolchain and `wasm32-unknown-unknown` target:

```sh
cargo build -p dekopon-context --example replay --locked
cargo build -p dekopon-context-wasm --target wasm32-unknown-unknown --release --locked
wasm-bindgen --target nodejs --out-dir /tmp/dekopon-context-binding target/wasm32-unknown-unknown/release/dekopon_context_wasm.wasm
node tests/context-replay/compare.cjs tests/context-replay/fixture.json target/debug/examples/replay /tmp/dekopon-context-binding/dekopon_context_wasm.js
```

`wasm-bindgen-cli` must match the locked `wasm-bindgen` package version. Outputs under `target/` and `/tmp` are generated and are not committed. The Node run checks actual WASM execution; a browser smoke and inference cassette are separate work.
