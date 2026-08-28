# dekopon-provider-sdk-testkit

An in-process fake broker for testing [Dekopon](https://github.com/dekopon-agents/dekopon) provider
components.

A provider's behavior only fully exists when its compiled component runs against a host. HTTP
providers can approximate that natively by injecting a transport closure; storage providers cannot,
because `dekopon-provider-storage` exposes free functions that call the WIT import directly and
those bindings expand to `unreachable!()` off `wasm32`. This crate closes that gap by running the
real component.

It is a *fake broker*, not a fake host. Cedar policy and the owner-authored constraint catalog are
skipped — `FakeBroker` mints its own authorization through `AuthorizationGate`, which is the
allow-all equivalent, since Dekopon has no wildcard grant spelling. Everything below that line is
real: the same Wasmtime host, the same `StorageHost`, and by default the same `StorageLimits` a
deployment runs. A quota a test trips here is a quota production would have tripped.

```rust,no_run
use dekopon_provider_sdk_testkit::{FakeBroker, StorageAccess, StorageInterface};
use serde_json::json;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let broker = FakeBroker::builder()
    .component("turso-sql-provider.wasm")
    .provider("turso")
    .storage(StorageInterface::DurableFiles, StorageAccess::ReadWrite)
    .build()
    .await?;

broker
    .invoke("turso.exec", json!({"statements": ["CREATE TABLE t(a INTEGER)"]}))
    .await?;

// A separate invocation reaches the same durable namespace.
let rows = broker
    .invoke("turso.exec", json!({"statements": ["SELECT count(*) FROM t"]}))
    .await?;
# let _ = rows;
# Ok(())
# }
```

## Things it does for you that are easy to get wrong

- **Continuity is selectable and defaults to `Stable`.** `ContinuityPolicy`'s own default is
  `AuthorityBound`, which mints a fresh non-reusing generation whenever the *effective authority
  commitment* changes. This harness holds that commitment constant, so `AuthorityBound` addresses
  one generation here exactly as `Stable` does; `.continuity(…)` selects either. `Stable` is the
  default because it is the policy that survives an authority change, so a harness that later
  grows a varying authority surface keeps addressing one namespace instead of silently starting
  over.
- **A grant is minted per invocation and consumed by it.** Successive calls get fresh invocation
  ids and identical scope material, which is what keeps them addressing one namespace.
- **`StorageNamespace::Chat` is the only namespace the storage host will grant.** A provider with
  nothing to do with chat still needs a transport, channel, and conversation; those are pre-filled.
- **The namespace key must live outside the root, owner-only.** Written for you at `0600`, in a
  `TempDir` the `FakeBroker` owns.

## Requirements

Tests must use a multi-thread runtime — `#[tokio::test(flavor = "multi_thread")]`. The storage path
dispatches to `spawn_blocking`, and a current-thread runtime deadlocks waiting for a namespace
lease.

Pass `.compile_cache(dir)` when a suite loads the same component repeatedly. Cranelift is the whole
of a cold start; on a large component this is the difference between a suite that runs and one
nobody waits for.

## License

MIT OR Apache-2.0
