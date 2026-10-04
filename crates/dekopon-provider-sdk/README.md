# dekopon-provider-sdk

Rust guest SDK for Dekopon component providers. Declare a `Provider` with an
identity, command words, clap `Args`, and a tuple of typed `Capability` types.
Each capability declares a closed, serializable input, a `Needs` tuple of
broker-granted handles, a serializable output, and a typed failure code. The
SDK derives the manifest and command dispatch; `export!(P)` exports the current
`dekopon:provider/provider-cli@0.4.0` WIT world. `propose` and `describe` must
not use host imports: only an authorized `invoke` receives handles.

```rust,ignore
// In a provider component with a declared Provider type:
dekopon_provider_sdk::export!(MyProvider);
```

The SDK owns guest bindings for buffered HTTP, asset-backed request bodies,
wall and monotonic clocks, OS entropy, settings, storage, and child scripts. `provider::Http`, `Clock`,
`Monotonic`, `Random`, `Settings<T>`, `Storage<K>`, `Assets`, and `Spawn` are private-construction handles; the broker alone
grants their imports. `Clock` remains wall-only; `Random::fill` splits a buffer into 4096-byte host reads. `Bounded<N>` validates input by UTF-8 byte length, and
`Truncated<N>` cuts output on a character boundary. `manifest::<P>()`,
`call::<P>()` and `command::<P>()` use the same typed dispatch as the component
export. `Spawn::run(script, ChildStdin)` exposes a child stdout reader and a status
resource; `Child::wait(self)` consumes and drops that reader before waiting.
`ChildStdin` is `None`, `Inherit`, or `Reader(Stdin)`; running twice before a wait
is refused as `SpawnError::Busy`. The native handle currently refuses with
`ComponentHarnessRequired` until the native fake is wired; the component host
currently traps the spawn import as unavailable. Neither is a working child
execution in this intermediate commit. The monotonic clock includes elapsed
child wait time; the separate work clock will park during a child wait.
The wire response and published provider WIT remain unchanged.

See the repository-owned probes under `examples/providers/` for working typed
components, and `dekopon-provider-sdk-testkit` for native/component parity and
conformance checks. Previously published guest binding crates continue to serve
external providers until those providers migrate separately; they are not part
of this workspace.
