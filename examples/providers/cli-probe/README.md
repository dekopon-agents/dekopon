# Command-line provider probe

Generated import-free fixture compiled against the `dekopon:provider@0.3.0` `provider-cli` world: `describe`, `invoke`, and the `run-command` export. It is the checked-in guest that proves both hosts call `run-command` with a real component — the typed `(list<string>, option<string>) -> string` lowering, a piped value reaching the guest, and every outcome parsing — and it is built on the SDK's `clap` layer, the encouraged way to write a command-line provider. The hand-rolled baseline the SDK documents is checked in as [`memory-reservation-probe`](../memory-reservation-probe/README.md).

Its word is `probe`, and it behaves like a small program. The tree is declared once with `#[derive(Parser)]` against the clap the SDK re-exports, and `dekopon_provider_sdk::cli::run_command` renders clap's answers: `probe --help` and `probe --version` render on stdout at status 0; `probe bogus`, `probe count` with no argument, and a bare `probe` render clap's usage error on stderr at status 2; `probe upper --text hi` proposes the read-only `cli-probe.upper` capability; `probe upper -` proposes it with the value piped into the word. What clap cannot know is declined by the dispatch closure with a `usage` or `invalid-input` error naming the cause: `probe upper -` with nothing piped, or a `text` beyond 16 KiB. `count` and `reverse` follow the same shape. The exact help page is pinned by the native tests; the fixture's `Cargo.lock` pins the clap that renders it, and the SDK's clap has no `color` feature, so no escape sequence ever appears.

Native checks and regeneration:

```console
cargo fmt --manifest-path examples/providers/cli-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/cli-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/cli-probe/Cargo.toml
examples/providers/cli-probe/build.sh
wasm-tools validate examples/providers/cli-probe-provider.wasm
wasm-tools component wit examples/providers/cli-probe-provider.wasm
```

The decoded component must export exactly `describe`, `invoke`, and `run-command`, and import nothing; its dependency tree on `wasm32-unknown-unknown` carries clap and its derive macros but nothing named `wasm-bindgen`, `js-sys`, or `wasi`. Never edit the checked-in Wasm directly.
