# dekopon-provider-clock

Rust guest bindings for `dekopon:clock/wall@1.0.0` and, with the `monotonic` feature, `dekopon:clock/monotonic@1.1.0`. The default build retains only the old wall-clock import.

The crate is statically compiled into a provider component. It contains no clock of its own and no ambient I/O; it calls the component import, which only `dekopon-brokerd` implements.

```rust,ignore
let unix_millis = dekopon_provider_clock::now_unix_millis();
// With feature `monotonic` and a matching component-world import:
let elapsed_nanos = dekopon_provider_clock::now_nanos();
```

The read is valid inside `invoke` only. `describe` and `run-command` are pure by contract, so a component that reads the clock from any of them traps and that call fails. A command word such as `date` therefore proposes a capability from `run-command` and reads the clock when the broker invokes it. Wall-clock reads are recorded as `provider_clock_read`; monotonic reads as `provider_monotonic_read` with elapsed nanoseconds, inside the invocation's trace. The monotonic origin belongs to the invocation store and is set before component instantiation. Elapsed readings are nondecreasing, not guaranteed to advance on each call.

The provider's component world imports `dekopon:clock/wall@1.0.0` beside its `dekopon:provider` exports, as [`clock-probe`](https://github.com/dekopon-agents/dekopon/tree/main/examples/providers/clock-probe) does. Monotonic consumers additionally import `dekopon:clock/monotonic@1.1.0`; a world can instead use `wall@1.1.0` if it wants the new package's wall interface. A broker older than the release that added the import refuses such a component at load with an unsatisfied import.
