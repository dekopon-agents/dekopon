# clock probe

A typed Rust provider fixture for `dekopon:provider@0.4.0` with `dekopon:clock/wall@1.1.0`, `dekopon:clock/monotonic@1.1.0`, and `dekopon:random/source@0.1.0` imports. The `date` word proposes `clock-probe.now`; `date --services` additionally reads elapsed nanoseconds and eight OS entropy bytes during invoke. `run-command` stays pure. Without the flag, invoke outputs `{"unixMillis": n, "rfc3339": "YYYY-MM-DDTHH:MM:SSZ"}`; with it, the output also includes `monotonicNanos` and `entropyBytes` (count only, never the bytes). Wall readings past year 9999 are refused. Imports do not grant broker authority.

This fixture is not packaged in the container image. Run native checks:

```console
cargo fmt --manifest-path examples/providers/clock-probe/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/clock-probe/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/clock-probe/Cargo.toml
```

Rebuild with `examples/providers/clock-probe/build.sh`. The decoded component must export exactly `describe`, `invoke`, and `run-command`, import stdio plus the three declared interfaces, and import no WASI interfaces. `clock-raw-probe` remains wall-only.
