# Provider fixtures and standalone providers

Dekopon core keeps only host-conformance fixtures in this directory. Providers with their own
behavior, dependency graph, issues, and release cadence live in standalone repositories. Core does
not track their source or generated Wasm.

Fetch the components local workspace tests need:

```console
ci/fetch-external-provider-components.sh examples/providers
```

The script downloads each release asset and sidecar, requires the checksum and byte length pinned
in core, and writes ignored fixture files at the paths tests expect. Set
`DEKOPON_VERIFY_PROVIDER_ATTESTATIONS=1` to additionally require GitHub artifact attestations.
These generated local files must never be committed.

Standalone providers this tree consumes at pinned releases:

- [`dekopon-provider-jsonplaceholder`](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder)
  — bounded broker HTTP read and synthetic external-write operations.
- [`dekopon-provider-memory-chat`](https://github.com/dekopon-agents/dekopon-provider-memory-chat)
  — optional JSONL-only durable chat memory.
- [`dekopon-provider-gh`](https://github.com/dekopon-agents/dekopon-provider-gh) — the
  nineteen-capability GitHub provider fetched by image staging.
- [`dekopon-provider-skylight-private`](https://github.com/dekopon-agents/dekopon-provider-skylight-private)
  — public source for the opt-in unofficial private-API exploration: unreleased, unsupported,
  mock-only, and absent from default catalogs, images, policies, and deployments.
- [`dekopon-provider-turso-sql`](https://github.com/dekopon-agents/dekopon-provider-turso-sql) —
  SQLite-compatible SQL over `durable-files`, distributed outside core.

The remaining checked components are repository-owned fixtures. Each declares a command word,
because a model reaches a provider only through one and a broker refuses to start with a provider
whose capabilities no word reaches:

- [`cli-probe/`](cli-probe/) is the import-free typed SDK guest built on clap:  its `probe` word renders clap's help and usage errors, reads a piped value, and proposes
  its three read-only capabilities.
- [`clock-probe/`](clock-probe/) uses typed SDK clock needs for its `date` word and invocation;
  its separate raw fixture retains the clock-read-during-command refusal. It is never packaged.
- [`http-probe/`](http-probe/) exercises typed SDK HTTP and asset handles; its `httpprobe` word proposes
  `fetch`, `conditional-write`, and `purge` from flags. Its `conditional-write` capability keeps
  two-call host budgets, per-call evidence, and etag-guarded writes covered without public
  network access.
- [`memory-reservation-probe/`](memory-reservation-probe/) is a single import-free raw-bindings
  adversarial fixture for memory-route escape; it is never packaged.
- [`storage-probe/`](storage-probe/) is the typed SDK durable-files conformance fixture;
  its `storageprobe` word proposes the run. It is never packaged in a scanned image directory.

Regenerate only repository-owned fixtures with their `build.sh`, each of which calls the shared
[`build-component.sh`](build-component.sh). That script reads the compiler from
[`rust-toolchain.toml`](../../rust-toolchain.toml) and wasm-tools from
[`ci/toolchain.env`](../../ci/toolchain.env) and refuses any other version. Never edit a `.wasm`
file directly.
[`JSONPLACEHOLDER.md`](JSONPLACEHOLDER.md) describes the standalone JSONPlaceholder provider's
linking constraints.
