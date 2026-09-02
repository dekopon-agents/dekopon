# Provider fixtures and standalone providers

Dekopon core keeps only host-conformance fixtures in this directory. Providers with their own
behavior, dependency graph, issues, and release cadence live in standalone repositories. Core does
not track their source or generated Wasm.

Fetch the exact v0.1.0 components needed by local workspace tests:

```console
ci/fetch-external-provider-components.sh examples/providers
```

The script downloads each release asset and sidecar, requires the checksum and byte length pinned
in core, and writes ignored fixture files at the historical paths expected by tests. Set
`DEKOPON_VERIFY_PROVIDER_ATTESTATIONS=1` to additionally require GitHub artifact attestations.
These generated local files must never be committed.

Standalone providers consumed at exact v0.1.0:

- [`dekopon-provider-echo`](https://github.com/dekopon-agents/dekopon-provider-echo) — import-free
  echo and deterministic Unicode message transformations;
  [release](https://github.com/dekopon-agents/dekopon-provider-echo/releases/tag/v0.1.0).
- [`dekopon-provider-jsonplaceholder`](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder)
  — bounded broker HTTP read and synthetic external-write operations;
  [release](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder/releases/tag/v0.1.0).
- [`dekopon-provider-memory-chat`](https://github.com/dekopon-agents/dekopon-provider-memory-chat)
  — optional JSONL-only durable chat memory;
  [release](https://github.com/dekopon-agents/dekopon-provider-memory-chat/releases/tag/v0.1.0).
- [`dekopon-provider-gh`](https://github.com/dekopon-agents/dekopon-provider-gh) — the
  nineteen-capability GitHub provider fetched by image staging at its pinned release.
- [`dekopon-provider-skylight-private`](https://github.com/dekopon-agents/dekopon-provider-skylight-private)
  — public source for the opt-in unofficial private-API exploration; it remains unreleased,
  unsupported, mock-only, and absent from default catalogs, images, policies, and deployments.
- [`dekopon-provider-turso-sql`](https://github.com/dekopon-agents/dekopon-provider-turso-sql) —
  SQLite-compatible SQL over `durable-files`, distributed outside core.

The remaining checked components are repository-owned fixtures:

- [`cli-probe/`](cli-probe/) is the import-free `run-command` guest: its `probe` word renders
  help and usage errors, reads a piped value, and proposes its three read-only capabilities.
- [`http-probe/`](http-probe/) composes provider exports with
  `dekopon:http/client@1.0.0`. Its `conditional-write` capability keeps two-call host budgets,
  per-call evidence, and etag-guarded writes covered without public network access.
- [`memory-reservation-probe/`](memory-reservation-probe/) is an import-free malicious
  chat-memory-route fixture and is never packaged.
- [`provider-v0-1-compat/`](provider-v0-1-compat/) pins compatibility with the immutable
  two-export `dekopon:provider@0.1.0` world.
- [`provider-v0-2-compat/`](provider-v0-2-compat/) pins compatibility with the immutable
  `dekopon:provider@0.2.0` `provider-commands` world and its legacy `resolve-command` export.
- [`storage-probe/`](storage-probe/) is the durable-files conformance fixture and is never packaged
  in a scanned image directory.

Regenerate only repository-owned fixtures with their `build.sh`, each of which passes its pinned
Rust toolchain to the shared [`build-component.sh`](build-component.sh); that script refuses any
other `rustc` and any `wasm-tools` other than 1.236.1. Never edit a `.wasm` file directly.
[`JSONPLACEHOLDER.md`](JSONPLACEHOLDER.md) describes the standalone JSONPlaceholder provider's
linking constraints.
