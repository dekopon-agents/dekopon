# dekopon-run

One-shot, immediate-mode execution for read-only Dekopon WebAssembly providers.

- `dekopon-run inspect` validates provider manifests.
- `dekopon-run invoke` directly invokes and times a capability.
- `dekopon-run prompt` exposes loaded capabilities as tools to an OpenAI-compatible chat-completions endpoint.
- `--trace <PATH>` exports Chrome/Perfetto-compatible spans.

Every provider call uses a fresh bounded Wasmtime store with no WASI or custom imports. The checked-in component is generated from the Rust [`Provider`](../dekopon-provider-sdk/README.md) implementation at [`../../examples/providers/echo/src/lib.rs`](../../examples/providers/echo/src/lib.rs):

```console
cargo run -p dekopon-run -- inspect \
  --provider examples/providers/echo-provider.wasm
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}'
```

This is an experimental development runner, not a daemon or authorization broker. It rejects local and external writes and does not resolve provider credentials. See [`../../docs/run.md`](../../docs/run.md) for the full contract.
