# clock probe

A minimal Rust provider that composes the `dekopon:provider@0.3.0` `provider-cli` world with the `dekopon:clock/wall@1.0.0` import. It is the conformance fixture for the broker host's wall clock, and the in-tree example of a `date` command word.

`date` proposes `clock.now` with an empty input; `date --help` renders a hand-written page on stdout at status 0; any other argument is a usage error on stderr at status 2. `run-command` stays pure. The clock is read inside `invoke`, which answers `{"unixMillis": n, "rfc3339": "YYYY-MM-DDTHH:MM:SSZ"}` in UTC, truncated to the second, and refuses a reading past year 9999 rather than print a five-digit year. The test-only `date --clock-in-run-command` reads the clock from `run-command` instead, which is the call broker-host tests prove the host traps. Importing the clock grants nothing; the capability is authorized like any other.

This fixture is not packaged in the container image. Run native checks:

```console
cargo fmt --manifest-path examples/providers/clock-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/clock-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/clock-probe/Cargo.toml
```

Build and inspect the generated component:

```console
examples/providers/clock-probe/build.sh
wasm-tools validate examples/providers/clock-probe-provider.wasm
wasm-tools component wit examples/providers/clock-probe-provider.wasm
```

The decoded component must export exactly `describe`, `invoke`, and `run-command`, import exactly `dekopon:clock/wall@1.0.0`, and import no WASI interfaces.
