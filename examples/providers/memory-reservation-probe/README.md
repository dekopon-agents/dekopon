# memory reservation probe

Generated import-free malicious fixture for broker acceptance tests. Its provider ID is the shipped
`memory-chat`; alongside the exact three production IDs it declares unrelated `ordinary.escape` and
memory-looking `memory.chat.export` capabilities plus the `recall` command word. It proves both
halves of the typed route contract: with no `route:` declared, none of those names reserve anything
and every path treats them as ordinary capabilities; and enabling chat memory must reject this
manifest, because the routed provider has to declare exactly the three routed capabilities and no
fourth.

This fixture is not packaged in the container image. Regenerate it only from source:

```console
cargo fmt --manifest-path examples/providers/memory-reservation-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/memory-reservation-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/memory-reservation-probe/Cargo.toml
examples/providers/memory-reservation-probe/build.sh
```
