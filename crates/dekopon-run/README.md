# dekopon-run

One-shot direct execution for read-only Dekopon WebAssembly providers plus an explicit unprivileged client for a separate broker.

- `dekopon-run inspect` validates direct provider manifests.
- `dekopon-run invoke` directly invokes and times an import-free read-only capability.
- `dekopon-run shell` runs one sandboxed [`dekopon-shell`](../dekopon-shell/README.md) script whose command words dispatch to provider capabilities instead of operating-system processes.
- `dekopon-run prompt` gives an OpenAI-compatible endpoint or a ChatGPT/Codex subscription one scripting tool instead of one tool per capability, optionally reaching a running broker with `--broker` for capabilities direct mode cannot serve.
- `dekopon-run broker capabilities` inspects exact policy visible to the authenticated Unix peer.
- `dekopon-run broker invoke` submits one identity-free, caller-ID-bearing proposal to `dekopon-brokerd`.
- `dekopon auth chatgpt` manages Dekopon's isolated subscription login.
- `--trace <PATH>` exports Chrome/Perfetto-compatible spans without inputs or outputs.
- `--otlp-endpoint <URL>` exports correlated OTLP/HTTP protobuf traces and audit-safe lifecycle logs; standard OTLP header environment variables carry receiver authentication and routing.

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

OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:5080/api/default \
OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20<INGESTION_TOKEN>,stream-name=dekopon' \
  cargo run -p dekopon-run -- invoke \
    --provider examples/providers/echo-provider.wasm \
    echo.echo --input '{"message":"observed"}'
```

The generic endpoint receives `/v1/traces` and `/v1/logs` suffixes. Prompts, model responses, model-authored script text and its output, provider input/output, credentials, and raw errors are excluded. See [`../../docs/observability.md`](../../docs/observability.md) for the signal model and [`../../examples/otel-traces/`](../../examples/otel-traces/README.md) for the single-container OpenObserve example.

Broker mode is deliberately separate and never accepts a principal, actor, policy, constraint, credential, or `AuthorizedInvocation` argument. The server derives identity from Unix peer credentials. The client validates private socket metadata plus connected peer credentials against a trusted server UID.

`--socket` and `--server-uid` are optional. The socket resolves from `--socket`, then `$DEKOPON_BROKER_SOCKET`, then `$XDG_RUNTIME_DIR/dekopon/broker.sock`, then `$HOME/.local/run/dekopon/broker.sock`, and fails with actionable guidance when none apply; candidates are never probed for existence, so a stopped daemon reports a connect failure against the exact resolved path. The server UID defaults to the caller's own effective UID, matching the single owner-UID trust domain of a per-user broker. Pass both explicitly for a broker running under a dedicated service account:

```console
dekopon-run broker capabilities

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
