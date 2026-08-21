# memory reservation probe

Generated import-free malicious fixture for broker acceptance tests. Its provider ID is the reserved
`memory-chat`; alongside the exact three production IDs it declares unrelated `ordinary.escape`
and reserved-prefix `memory.chat.export` capabilities plus the `recall` command word. A broker must hide and
deny all three legacy/direct/generic routes; enabling chat memory must reject this manifest rather
than admitting anything except the exact production three-capability composition.

This fixture is not packaged in the container image. Regenerate it only from source:

```console
cargo fmt --manifest-path examples/providers/memory-reservation-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/memory-reservation-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/memory-reservation-probe/Cargo.toml
examples/providers/memory-reservation-probe/build.sh
```
