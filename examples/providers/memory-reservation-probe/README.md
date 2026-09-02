# memory reservation probe

Generated import-free malicious fixture for broker acceptance tests. Its provider ID is the shipped
`memory-chat`; alongside the exact three production IDs it declares unrelated `ordinary.escape` and
memory-looking `memory.chat.export` capabilities plus the `recall` command word. It proves both
halves of the typed route contract: with no `route:` declared, none of those names reserve anything
and every path treats them as ordinary capabilities; and enabling chat memory must reject this
manifest, because the routed provider has to declare exactly the three routed capabilities and no
fourth.

It is also the checked-in hand-rolled `run-command` guest, compiled against the
`dekopon:provider@0.3.0` `provider-cli` world (`describe`, `invoke`, `run-command`) with no
argument parser: values are shifted out of argv by hand, which is the clap-free baseline the SDK's
`Provider::run_command` contract promises. `recall --help` renders a short hand-written page on
stdout at status 0; `recall`, alone or with any positional arguments, proposes `ordinary.escape`
with an empty input, exactly as its legacy `resolve-command` rewrite did, so every broker
reservation test keeps its behaviour; any other flag is declined with a `usage` error. The piped
value is ignored. The `clap`-layer counterpart is [`cli-probe`](../cli-probe/README.md).

This fixture is not packaged in the container image. Regenerate it only from source:

```console
cargo fmt --manifest-path examples/providers/memory-reservation-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/memory-reservation-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/memory-reservation-probe/Cargo.toml
examples/providers/memory-reservation-probe/build.sh
wasm-tools validate examples/providers/memory-reservation-probe-provider.wasm
wasm-tools component wit examples/providers/memory-reservation-probe-provider.wasm
```

The decoded component must export exactly `describe`, `invoke`, and `run-command`, and import
nothing. Never edit the checked-in Wasm directly.
