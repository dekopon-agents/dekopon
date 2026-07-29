# Immediate provider runner

`dekopon-run` is an **experimental current** one-shot runner for developing and measuring read-only Dekopon providers. It is separate from the `dekopon` operator CLI and is not a daemon, policy engine, authorization broker, or production provider boundary.

## Commands

```text
dekopon-run inspect --provider <COMPONENT>...
dekopon-run invoke --provider <COMPONENT>... <CAPABILITY> [--input <JSON> | --input-file <PATH>] [--repeat <COUNT>]
dekopon-run prompt --provider <COMPONENT>... --model <MODEL> [--endpoint <URL> | --chatgpt-subscription] <PROMPT>
dekopon auth chatgpt <login | status | logout>
```

Each command builds one `ProviderRegistry`, compiles every selected component once, and retains that machine code only for the registry's lifetime. There is no persistent compilation cache between processes. Description and invocation calls receive a fresh Wasmtime store and component instance with configured memory, fuel, wall-clock, input, and output limits; one shared runtime mutex serializes component calls. Repeating `--provider` creates one deterministic capability registry, and duplicate provider or capability IDs fail before invocation. Success exits `0`, runtime/model/provider failures exit `1`, and Clap usage failures exit `2`.

The checked-in Rust echo provider is immediately runnable:

```console
cargo run -p dekopon-run -- inspect \
  --provider examples/providers/echo-provider.wasm

cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}'

cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.ransom-case --input '{"message":"Hello, World!"}'

time target/release/dekopon-run invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}' --repeat 100
```

The example provider also exposes `echo.reverse`, `echo.upcase`, and `echo.downcase`; all four transforms accept and return `{"message":"..."}`. `invoke` emits a JSON report containing the routed provider, capability, iteration count, warm invocation timings, and final raw JSON output. That output is not broker evidence or an `InvocationResult`. Shell `time` also includes process startup and component compilation.

## Prompt mode

Prompt mode supports either an OpenAI-compatible Chat Completions endpoint or a ChatGPT/Codex subscription. Both backends expose the same bounded provider tool loop.

### OpenAI-compatible endpoints

The default endpoint is `http://127.0.0.1:11434/v1`, suitable for an Ollama-compatible local server:

```console
cargo run -p dekopon-run -- prompt \
  --provider examples/providers/echo-provider.wasm \
  --model qwen3 \
  'Use the echo tool with the message hello'
```

Provider capability IDs are converted into deterministic OpenAI-compatible function names. Model arguments remain untrusted JSON, can select only offered capabilities, and are checked against the host's object-input requirement before invocation. Capability schemas are sent to the model, but the host does not perform general JSON Schema validation; each provider must validate its own required fields, types, and operation-specific constraints. Tool results are returned to the model until it emits final text or `--max-steps` is reached. A single model turn is capped at 32 tool calls to bound adversarial endpoint fan-out.

For authenticated endpoints, `--api-key-env <NAME>` names an environment variable read as a bearer token; it defaults to `OPENAI_API_KEY`. The token is never sent to a provider or recorded as a tracing field, HTTP redirects are disabled, and bearer tokens require HTTPS except for loopback HTTP endpoints.

### ChatGPT/Codex subscription

Dekopon implements OpenAI's Codex device authorization and streaming Responses protocol directly; OpenClaw, the Codex CLI, and pi are not runtime dependencies. Sign in once:

```console
target/release/dekopon auth chatgpt login
```

The command prints `https://auth.openai.com/codex/device` and a short code, waits for authorization, then stores refreshable credentials in Dekopon's own credential file. Check or remove that login with:

```console
target/release/dekopon auth chatgpt status
target/release/dekopon auth chatgpt logout
```

Then select the subscription backend explicitly:

```console
target/release/dekopon-run prompt \
  --provider examples/providers/echo-provider.wasm \
  --chatgpt-subscription \
  --model gpt-5.6-sol \
  'Use the echo tool with the message hello'
```

Use an exact model exposed to the signed-in Codex account; `gpt-5.5` is a recovery choice when the account does not expose GPT-5.6. Dekopon automatically refreshes an expiring access token and replays opaque encrypted reasoning items only in memory when a tool call requires another model turn.

The default credential file is `~/.config/dekopon/chatgpt-auth.json` (`0600` on Unix). `DEKOPON_CHATGPT_AUTH_FILE`, `dekopon auth chatgpt ... --auth-file`, or `dekopon-run prompt --chatgpt-auth-file` can override it. Dekopon intentionally never imports OAuth material from pi, OpenClaw, or the Codex CLI. The model request is sent only to `auth.openai.com` during login and `chatgpt.com/backend-api/codex/responses` during inference; those endpoints are fixed rather than user-configurable.

The subscription transport receives the prompt, system instruction, provider tool schemas, tool arguments, and tool results. Credentials are never passed to Wasm providers or trace fields. Subscription quotas and model availability remain controlled by OpenAI and are distinct from Platform API billing.

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

Build providers for `wasm32-unknown-unknown`, then componentize the embedded WIT metadata. The echo provider is a separate Cargo workspace, and its checked-in component is generated rather than hand-edited. The [echo-provider README](../examples/providers/echo/README.md) and [`development.md`](development.md) contain exact build and validation commands. A `wasm32-wasip2` build imports WASI and will be rejected because this host intentionally links no guest imports.

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
