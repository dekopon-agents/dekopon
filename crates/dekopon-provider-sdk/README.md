# dekopon-provider-sdk

Write a Dekopon provider: a Wasm component that gives agents new shell commands. The broker runs it
once per authorized invocation, in a fresh memory-bounded store. Its stdout is the result; a failure
exits with the capability's code and writes its message to stderr.

## The model

A `Provider` declares its command words, a clap grammar and a tuple of `Capability` types. When a
model's script runs one of those words, `propose` turns the arguments into one capability proposal.
It runs before authorization, so it reaches no host import. Only a call the broker authorizes reaches
`Capability::run`.

Each capability declares its `Needs`: handles the SDK constructs only inside an authorized call.
They are `Http`, `Clock`, `Monotonic`, `Random`, `Settings<T>`, `Storage<Jsonl>`,
`Storage<DurableFiles>`, `Assets`, and `Spawn`, which runs a child script through the gateway under
the same person, agent and budget. The host links no WASI, so a provider has no filesystem,
sockets or environment beyond these.

Never set an `Authorization` header. The broker's HTTP engine refuses one from the guest, holds each
request to the operator's grant and a Cedar check, then adds the operator's credential for that
destination. Trust the endpoint: it receives the credential.

[`cli-probe`](../../examples/providers/cli-probe/src/lib.rs) is a complete provider.

## Build

A provider is a `cdylib` crate that ends with `dekopon_provider_sdk::export!(MyProvider);`.

```console
cargo build --release --target wasm32-unknown-unknown
wasm-tools component new target/wasm32-unknown-unknown/release/my_provider.wasm -o my-provider.wasm
```

`dekopon_provider_sdk::skills!("my-provider/SKILL.md")` embeds Agent Skills from the crate's
`skills/` directory. They are model text and grant nothing.

## Test

Add `dekopon-provider-sdk-testkit` as a dev-dependency at exactly the SDK's version. `Native::<P>`
runs a capability in-process against scripted HTTP, clocks and children. `Harness::<P>::get(path)`
runs the built component on the real broker host against a loopback HTTPS script.
`conformance::<P>(path)` checks that the component's manifest, help, imports and input schemas match
the Rust declaration.

```rust
conformance::<MyProvider>("my-provider.wasm").expect("component matches its declaration");
let out = Harness::<MyProvider>::get("my-provider.wasm")
    .call("my-provider.upper", serde_json::json!({ "text": "hi" }))
    .expect("component runs");
assert_eq!(out.status, 0);
```

## Install

A provider does nothing until the operator grants it. In the broker configuration, list the
component, or pin it in a provider set that `dekopon-brokerd provider sync` resolves; give each
capability you want reachable a constraint set; and permit it in Cedar. The broker denies a
capability without a constraint set before Cedar is asked, and no policy widens a constraint set.
[`conditional-write`](../../examples/conditional-write/broker.yaml) grants two of the `http-probe`
provider's three capabilities. Run `dekopon-brokerd check` before restarting the broker.
