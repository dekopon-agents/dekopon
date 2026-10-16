# Synthetic context replay

This experimental replay takes already eligible, scripted records with exact routine/version representations and source eligibility. Each cassette request must match the record, ordinal, version, and config identity exactly. The response names the representation to render. The standalone replay adapter calls the portable source module owned by `dekopon-agent` to render the current journal body and JSON memory label, preserve split original user/assistant turns, and fit whole visible turns including label bytes; retained records remain visible separately in each snapshot. No inference request, network call, journal read, authority decision, or model request is produced.

Build and run from the repository root with the pinned Rust toolchain and `wasm32-unknown-unknown` target:

```sh
cargo build --manifest-path tests/context-replay/Cargo.toml --example native --locked
cargo build --manifest-path tests/context-replay/Cargo.toml --lib --target wasm32-unknown-unknown --release --locked
wasm-bindgen --target nodejs --out-dir /tmp/dekopon-context-binding tests/context-replay/target/wasm32-unknown-unknown/release/context_replay.wasm
node tests/context-replay/compare.cjs tests/context-replay/fixture.json tests/context-replay/target/debug/examples/native /tmp/dekopon-context-binding/context_replay.js
```

`wasm-bindgen-cli` must match the adapter’s locked `wasm-bindgen` package version. Outputs under `tests/context-replay/target/` and `/tmp` are generated and are not committed. The Node run checks actual WASM execution; a browser smoke and inference cassette are separate work.

The v2 synthetic fixture supplies eligibility explicitly; production journal selection, decider fallback, pins, and generation routing remain native. The portable module uses `serde` and `serde_json`, already present in both consumers. Decimal-string record IDs are parsed as `u64` in Rust before rendering, preserving label precision above JavaScript’s safe integer range.
