# Immediate provider runner

`dekopon-run` is an **experimental current** one-shot runner for developing and measuring read-only Dekopon providers. It is separate from the `dekopon` catalog CLI and is not a daemon, policy engine, authorization broker, or production provider boundary.

## Commands

```text
dekopon-run inspect --provider <COMPONENT>...
dekopon-run invoke --provider <COMPONENT>... <CAPABILITY> [--input <JSON> | --input-file <PATH>] [--repeat <COUNT>]
dekopon-run prompt --provider <COMPONENT>... --model <MODEL> [--endpoint <URL>] <PROMPT>
```

All commands compile each component once. Description and invocation calls receive a fresh Wasmtime store with configured memory, fuel, wall-clock, input, and output limits. Repeating `--provider` creates one deterministic capability registry; duplicate provider or capability IDs fail before invocation. Success exits `0`, runtime/model/provider failures exit `1`, and Clap usage failures exit `2`.

The checked-in Rust echo provider is immediately runnable:

```console
cargo run -p dekopon-run -- inspect \
  --provider examples/providers/echo-provider.wasm

cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}'

time target/release/dekopon-run invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}' --repeat 100
```

`invoke` emits a JSON report containing the routed provider, capability, iteration count, warm invocation timings, and final output. Shell `time` also includes process startup and component compilation.

## Prompt mode

Prompt mode sends OpenAI-compatible chat-completions requests. The default endpoint is `http://127.0.0.1:11434/v1`, suitable for an Ollama-compatible local server:

```console
cargo run -p dekopon-run -- prompt \
  --provider examples/providers/echo-provider.wasm \
  --model qwen3 \
  'Use the echo tool with the message hello'
```

Provider capability IDs are converted into deterministic OpenAI-compatible function names. Model arguments remain untrusted JSON, can select only offered capabilities, and are checked against the host's object-input requirement before invocation. Tool results are returned to the model until it emits final text or `--max-steps` is reached. A single model turn is capped at 32 tool calls to bound adversarial endpoint fan-out.

For authenticated endpoints, `--api-key-env <NAME>` names an environment variable read as a bearer token; it defaults to `OPENAI_API_KEY`. The token is never sent to a provider or recorded as a tracing field, HTTP redirects are disabled, and bearer tokens require HTTPS except for loopback HTTP endpoints.

## Rust provider interface

Provider source implements `dekopon_provider_sdk::Provider` and uses `export_provider!`. See [`../examples/providers/echo/src/lib.rs`](../examples/providers/echo/src/lib.rs).

The SDK adapter exposes [`../crates/dekopon-provider-sdk/wit/provider.wit`](../crates/dekopon-provider-sdk/wit/provider.wit):

```wit
world provider {
    export describe: func() -> string;
    export invoke: func(capability: string, input-json: string) -> string;
}
```

The strings carry strict typed manifest and response JSON. This keeps the first WIT surface deliberately small while the Rust trait and wire model stabilize.

Build providers for `wasm32-unknown-unknown`, then componentize the embedded WIT metadata. The echo-provider README contains exact commands. A `wasm32-wasip2` build imports WASI and will be rejected because this host intentionally links no guest imports.

## Tracing and limits

`--trace <PATH>` writes Chrome/Perfetto-compatible JSON containing runner, model, component compilation, description, and invocation spans. Prompt text, model responses, provider input/output, and bearer tokens are intentionally excluded from span fields.

Global bounds are configurable with:

- `--max-memory-bytes`
- `--max-input-bytes`
- `--max-output-bytes`
- `--fuel`
- `--timeout-ms`

The host supplies no WASI, filesystem, network, environment, clock, random, or credential imports. It accepts only capabilities declaring `read-only`. Consequently this path exercises pure provider computation; it does not grant read access to an external system despite the effect label.

## Authority limitation

A model tool call in `dekopon-run` is not an `AuthorizedInvocation`. Immediate mode performs no broker transition and must not be extended to provider credentials, host networking, local writes, or external writes. Those require the authenticated, policy-controlled, separately deployed broker described in [`design.md`](design.md) and [`security-model.md`](security-model.md).
