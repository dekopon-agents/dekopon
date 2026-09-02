# Provider 0.2 compatibility fixture

Generated import-free component compiled against the immutable `dekopon:provider@0.2.0` `provider-commands` world: `describe`, `invoke`, and the legacy `resolve-command` export. Its `wit/deps/provider.wit` is a frozen copy of that package, deliberately not a mirror of the current SDK file, and CI fails if the current package version appears under its `wit/`. Host compatibility tests load and run this artifact to prove a `resolve-command` guest keeps working on the same command path after `dekopon:provider@0.3.0` introduced `run-command`, without rebuilding historical providers.

It declares the `compat` command word; `compat echo` resolves to its read-only `provider-v0-2-compat.echo` capability with an empty input, and any other argv is declined with a `usage` error.

Regenerate with `./build.sh`; never edit the checked-in Wasm directly.
