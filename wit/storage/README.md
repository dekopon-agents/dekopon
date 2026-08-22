# `dekopon:storage@0.1.0`

Canonical WIT source for Dekopon's broker-owned, namespace-bound provider storage interfaces.

The `jsonl` interface offers invocation-transactional chunk reads, append, and replacement. The
`durable-files` interface offers engine-neutral positional files, rollback-journal lock levels,
bounded entropy, and bounded clocks. Neither interface exposes host paths, namespace selection,
SQL, sockets, environment variables, or WASI. An import is only a structural requirement; only
`dekopon-brokerd` can bind it to a freshly authorized storage grant.

Build and inspect the package from this directory with `wkg build` and `wasm-tools component wit`.
The canonical file and all guest/host mirrors listed in `docs/development.md` must remain
byte-identical.
