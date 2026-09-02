# dekopon-provider-storage

Feature-gated Rust guest bindings for `dekopon:storage@0.1.0`.

- `jsonl` exposes bounded size/chunk reads and invocation-transactional append/replace.
- `durable-files` exposes namespace-bound positional files, rollback-journal lock levels,
  durability modes, entropy, and clocks.
- the default feature set emits no storage import.

The crate contains bindings and ergonomic value types only. It has no host path, namespace,
authority, transaction, SQL, filesystem, socket, environment, or credential API. Constructing a
request never grants storage: `dekopon-brokerd` links only the interface selected by an exact
owner-authored storage constraint and commits mutations only after a valid successful provider
response.

```toml
# JSONL-only: exactly one storage interface import.
dekopon-provider-storage = { version = "0.12", default-features = false, features = ["jsonl"] }
```

`0.12` resolves once the interrupted `v0.12.0` crates.io publication is recovered, as described
under the root README's [crates.io](../../README.md#cratesio) section; until then `0.11.1` is the
newest `dekopon-provider-storage` on crates.io.

The in-tree [`storage-probe`](../../examples/providers/storage-probe/README.md) fixture depends on
the crate by `path` and selects `durable-files` the same way.
