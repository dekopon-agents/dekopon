# dekopon-run

One-shot direct execution for read-only Dekopon WebAssembly providers plus an explicit unprivileged client for a separate broker.

- `dekopon-run inspect` validates direct provider manifests.
- `dekopon-run invoke` directly invokes and times an import-free read-only capability.
- `dekopon-run prompt` exposes direct capabilities as tools to an OpenAI-compatible endpoint or a ChatGPT/Codex subscription.
- `dekopon-run broker capabilities` inspects exact policy visible to the authenticated Unix peer.
- `dekopon-run broker invoke` submits one identity-free, caller-ID-bearing proposal to `dekopon-brokerd`.
- `dekopon auth chatgpt` manages Dekopon's isolated subscription login.
- `--trace <PATH>` exports Chrome/Perfetto-compatible spans without inputs or outputs.

Every **direct** provider call uses a fresh bounded Wasmtime store with no WASI or custom imports. The checked-in component is generated from the Rust [`Provider`](../dekopon-provider-sdk/README.md) implementation at [`../../examples/providers/echo/src/lib.rs`](../../examples/providers/echo/src/lib.rs):

```console
cargo run -p dekopon-run -- inspect \
  --provider examples/providers/echo-provider.wasm
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}'
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.reverse --input '{"message":"stressed"}'
```

Broker mode is deliberately separate and never accepts a principal, actor, policy, constraint, credential, or `AuthorizedInvocation` argument. The server derives identity from Unix peer credentials. The client requires the trusted server UID and validates private socket metadata plus connected peer credentials:

```console
dekopon-run broker capabilities \
  --socket "$HOME/.local/run/dekopon/broker.sock" \
  --server-uid "$(id -u)"

dekopon-run broker invoke \
  --socket "$HOME/.local/run/dekopon/broker.sock" \
  --server-uid "$(id -u)" \
  --invocation-id invoke-example-001 \
  --trace-id trace-example-001 \
  jsonplaceholder.posts.get --input '{"postId": 7}'
```

Invocation IDs are mandatory caller-generated replay keys and must not be reused. The client does not retry; a lost external-write response is an unknown outcome that requires audit review. Broker invocation JSON is printed for every terminal outcome; denied/failed outcomes exit `1`. Broker mode uses a fresh connection per operation with explicit frame and deadline bounds; immediate Wasm limit flags are absent from broker subcommands because only the broker chooses provider constraints.

ChatGPT subscription mode uses OpenAI's device authorization and Codex Responses transport directly; it does not import credentials from pi, OpenClaw, or the Codex CLI. `dekopon-run` remains an unprivileged client, not an authorization broker. Direct mode rejects local and external provider writes and never resolves provider credentials. See [`../../docs/run.md`](../../docs/run.md) and [`../../docs/broker-http.md`](../../docs/broker-http.md) for the full contracts.
