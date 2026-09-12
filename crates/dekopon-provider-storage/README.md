# dekopon-provider-storage

Feature-gated Rust guest bindings for `dekopon:storage@0.1.0`.

- `jsonl` exposes bounded size/chunk reads and per-call append/replace.
- `durable-files` exposes namespace-bound positional files, rollback-journal lock levels,
  durability modes, entropy, and clocks.
- the default feature set emits no storage import.

The crate contains bindings and ergonomic value types only. It has no host path, namespace,
authority, transaction, SQL, filesystem, socket, environment, or credential API. Constructing a
request never grants storage: `dekopon-brokerd` links only the interface selected by an exact
owner-authored storage constraint. Writes take effect per host call; completed writes survive
invocation failure, without invocation-wide rollback.

```toml
# JSONL-only: exactly one storage interface import.
dekopon-provider-storage = { version = "0.13", default-features = false, features = ["jsonl"] }
```

When crates.io does not carry `0.13`, take the tap or the archives
([crates.io](../../README.md#cratesio)).

The in-tree [`storage-probe`](../../examples/providers/storage-probe/README.md) fixture depends on
the crate by `path` and selects `durable-files` the same way.
