# Synthetic context replay

This experimental replay takes already eligible, scripted records. Each cassette request must match the record, ordinal, version, and config identity exactly. The response names the representation to render. The standalone replay adapter calls `dekopon-core::context` to render that representation and fit whole visible turns; retained records remain visible separately in each snapshot. No inference request, network call, journal read, authority decision, or model request is produced.

Build and run from the repository root with the pinned Rust toolchain and `wasm32-unknown-unknown` target:

```sh
cargo build --manifest-path tests/context-replay/Cargo.toml --example native --locked
cargo build --manifest-path tests/context-replay/Cargo.toml --lib --target wasm32-unknown-unknown --release --locked
wasm-bindgen --target nodejs --out-dir /tmp/dekopon-context-binding tests/context-replay/target/wasm32-unknown-unknown/release/context_replay.wasm
node tests/context-replay/compare.cjs tests/context-replay/fixture.json tests/context-replay/target/debug/examples/native /tmp/dekopon-context-binding/context_replay.js
```

`wasm-bindgen-cli` must match the adapter’s locked `wasm-bindgen` package version. Outputs under `tests/context-replay/target/` and `/tmp` are generated and are not committed. The Node run checks actual WASM execution; a browser smoke and inference cassette are separate work.
