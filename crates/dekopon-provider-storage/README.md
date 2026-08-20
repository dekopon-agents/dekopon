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
# Unreleased repository fixture: JSONL-only means exactly one storage interface import.
dekopon-provider-storage = { path = "../../../crates/dekopon-provider-storage", default-features = false, features = ["jsonl"] }
```
