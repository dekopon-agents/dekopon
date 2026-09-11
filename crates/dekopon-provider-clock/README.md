# dekopon-provider-clock

Rust guest bindings for the `dekopon:clock@1.0.0` WebAssembly Component Model interface: the broker host's wall clock.

The crate is statically compiled into a provider component. It contains no clock of its own and no ambient I/O; it calls the component import, which only `dekopon-brokerd` implements.

```rust,ignore
let unix_millis = dekopon_provider_clock::now_unix_millis();
```

The read is valid inside `invoke` only. `describe`, `run-command`, and `resolve-command` are pure by contract, so a component that reads the clock from any of them traps and that call fails. A command word such as `date` therefore proposes a capability from `run-command` and reads the clock when the broker invokes it. Each read is recorded as a `provider_clock_read` event inside the invocation's `provider.invoke` span.

The provider's component world imports `dekopon:clock/wall@1.0.0` beside its `dekopon:provider` exports, as [`clock-probe`](https://github.com/dekopon-agents/dekopon/tree/main/examples/providers/clock-probe) does. A broker older than the release that added the import refuses such a component at load with an unsatisfied import.
