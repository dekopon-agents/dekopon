# Command-line provider probe

Generated import-free fixture compiled against the `dekopon:provider@0.3.0` `provider-cli` world: `describe`, `invoke`, and the `run-command` export. It is the checked-in guest that proves both hosts call `run-command` with a real component — the typed `(list<string>, option<string>) -> string` lowering, a piped value reaching the guest, and every outcome parsing — where the other command-word fixtures only exercise the legacy `resolve-command` path.

Its word is `probe`, and it behaves like a small program. `probe --help` and `probe --version` render on stdout at status 0; `probe upper --text hi` proposes the read-only `cli-probe.upper` capability; `probe upper -` proposes it with the value piped into the word; `probe upper -` with nothing piped, a missing argument, or a `text` beyond 16 KiB is a usage error on stderr at status 2; and an unknown subcommand is declined with a `usage` error. `count` and `reverse` follow the same shape. The argument handling is hand-rolled on purpose: it is the clap-free baseline the SDK documents, and a clap-based layer is a later addition.

Native checks and regeneration:

```console
cargo fmt --manifest-path examples/providers/cli-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/cli-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/cli-probe/Cargo.toml
examples/providers/cli-probe/build.sh
wasm-tools validate examples/providers/cli-probe-provider.wasm
wasm-tools component wit examples/providers/cli-probe-provider.wasm
```

The decoded component must export exactly `describe`, `invoke`, and `run-command`, and import nothing. Never edit the checked-in Wasm directly.
