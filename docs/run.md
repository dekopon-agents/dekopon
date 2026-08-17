# Direct provider runner and broker client

`dekopon-run` is an **experimental current** one-shot runner for developing and measuring read-only import-free providers plus an explicit unprivileged client for `dekopon-brokerd`. It is separate from the `dekopon` operator CLI and is not a daemon, policy engine, authorization broker, or production provider boundary.

## Commands

```text
dekopon-run inspect --provider <COMPONENT>...
dekopon-run invoke --provider <COMPONENT>... <CAPABILITY> [--input <JSON> | --input-file <PATH>] [--repeat <COUNT>]
dekopon-run prompt --provider <COMPONENT>... --model <MODEL> [--endpoint <URL> | --chatgpt-subscription] [--broker [--socket <PATH>] [--server-uid <UID>]] [--curl-capability <CAPABILITY>] <PROMPT>
dekopon-run shell --provider <COMPONENT>... [--curl-capability <CAPABILITY>] <SCRIPT>
dekopon-run broker capabilities [--socket <PATH>] [--server-uid <UID>]
dekopon-run broker invoke [--socket <PATH>] [--server-uid <UID>] --invocation-id <ID> --trace-id <ID> <CAPABILITY> [--input <JSON> | --input-file <PATH>]
dekopon auth chatgpt <login | status | logout>
```

Each direct `inspect`, `invoke`, or `prompt` command builds one `ProviderRegistry`, compiles every selected component once, and retains that machine code only for the registry's lifetime. There is no persistent compilation cache between processes. Description and invocation calls receive a fresh Wasmtime store and component instance with configured memory, fuel, wall-clock, input, and output limits; one shared runtime mutex serializes component calls. Repeating `--provider` creates one deterministic capability registry, and duplicate provider or capability IDs fail before invocation. Success exits `0`, runtime/model/provider failures exit `1`, and Clap usage failures exit `2`. Broker invocations always print the typed result; `Denied` or `Failed` outcomes exit `1`, while `Succeeded` exits `0`.

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

The example provider also exposes `echo.reverse`, `echo.upcase`, and `echo.downcase`; all four transforms accept and return `{"message":"..."}`. Direct `invoke` emits a JSON report containing the routed provider, capability, iteration count, warm invocation timings, and final raw JSON output. That output is not broker evidence or an `InvocationResult`. Shell `time` also includes process startup and component compilation.

## Shell mode

`dekopon-run shell` runs one script through [`dekopon-shell`](../crates/dekopon-shell/), a sandboxed bash-flavored interpreter whose command words dispatch to provider capabilities instead of operating-system processes. It contacts no model. The same interpreter backs the single `bash` tool prompt mode offers, so `shell` is the way to develop and replay a script by hand exactly as a model would run it.

```console
cargo run -p dekopon-run -- shell \
  --provider examples/providers/echo-provider.wasm \
  'for word in alpha beta; do echo.upcase --message $word | jq -r .message; done'
```

A granted capability is callable as a bare command, and `cap` is always available:

```console
cargo run -p dekopon-run -- shell \
  --provider examples/providers/echo-provider.wasm \
  'cap --list | jq -r ".[]"'
```

Every shell variable is a JSON value rather than bash text, so capability inputs and outputs need no marshaling. `NAME --kebab-case value` flags become camelCase JSON keys, matching the capability input convention used everywhere else. A whole-right-hand-side `x=$(cmd)` keeps the command's structured value, which is a deliberate documented deviation from bash; interpolating `$( )` anywhere else coerces to text exactly as bash does. `|` delivers one structured value to the next command rather than a byte stream, and `>`/`>>` write to named in-memory buffers read back only by `cat`, never to files.

The language keeps `if`/`elif`/`else`, `for`, `while`, `until`, `case`/`esac`, `break`/`continue` with levels, functions with `$1`/`$@`/`$*`/`$#`, `shift`, and `local`, `&&`/`||`/`;`/`|`, a leading `!` to invert a pipeline, `$?`, `$(( ))` arithmetic, both quoting forms, here-documents (`<<EOF`, `<<-EOF`, and the literal `<<'EOF'`), and `#` comments.

Dropped constructs fall into two groups, and the difference matters:

- **Rejected loudly**, by name, as a parse or run failure: backticks (use `$( )`), job control (a trailing `&`), subshells, `(( ))`, `name=(a b c)`, C-style `for (( ))`, `[[ ]]`, `set` and its options, file-descriptor redirection (`2>`, `>&2`), here-strings (`<<<`), `case` fall-through (`;&`, `;;&`), process substitution, `eval`, `exec`, `source`, `declare`, `export`, bash's array emulation, `${name:-default}`-style expansions, and regex metacharacters in a `grep`/`sed` pattern or glob metacharacters in a `case` pattern.
- **Inert literals**, indistinguishable from ordinary text: globbing (`*`, `?`, `[abc]`), brace expansion (`{a,b}`), tilde expansion (`~`), and POSIX IFS word splitting. There is no filesystem to glob against and no `IFS` to split on, so there is nothing for these to be rejected against; an unquoted expansion holding a JSON array is what produces multiple words here.

Everything in the first group fails rather than doing something else, so a model can never believe something happened that did not. That rule reaches inside kept constructs too: a `case` pattern is literal text, so `*)` is still the default branch but `*.json)` is a parse error rather than a silent mismatch — the same treatment a regex metacharacter already gets in a `grep` pattern, and for the same reason. A here-document's body arrives as one JSON string, since a block of literal text is a string in this value model; pipe it through `jq 'fromjson'` when it holds JSON, because a body that merely looks like JSON is never parsed behind the script's back.

Script bounds are separate from the Wasm bounds, because a script decides how many component calls happen:

- `--shell-max-steps`
- `--shell-max-recursion-depth`
- `--shell-max-output-bytes`
- `--shell-max-output-lines`
- `--shell-timeout-ms`
- `--shell-max-capability-calls`
- `--shell-max-value-bytes`
- `--shell-allow-clock`

`--shell-max-value-bytes` is the memory bound: it counts, cumulatively across the run, the bytes a script materializes into variables, buffers, and substitutions, so a script that is cheap in steps and expensive in memory (`x="$x$x"` in a loop) is stopped by something. Grammar nesting depth has a fixed ceiling that is not configurable, because it is a property of the parser's stack rather than of the script's budget; deeply nested `$( $( ... ) )` is a syntax error.

`--shell-allow-clock` is the odd one out: a permission rather than a ceiling, and the only ambient authority the interpreter can be granted. Reading the wall clock has no capability to go through — there is no provider to authorize "what time is it", and inventing one would be a fiction — so it is an explicit, off-by-default operator flag instead. With it unset, `date` reports "command not found" exactly as an ungranted capability does, rather than returning a fabricated time a script would act on. With it set, `date` renders the current UTC time as `+%s` or an ISO-8601 instant and nothing else; there is no `strftime`, and it can neither set the clock nor convert another zone.

The interpreter's variable namespace is seeded only by the script's own assignments; it never reads the host process environment, so `$PATH` and `$OPENAI_API_KEY` are unset inside a script. That covers `jq` too: jaq's `env` and `now` filters are not linked, so `jq -r env.SECRET` reports an undefined filter. `--shell-allow-clock` does not reopen either one — it grants the `date` builtin a wall-clock reading and nothing more, and `date` consults no environment variable, so `TZ` cannot be observed through it and the output is always UTC. Output is truncated to the configured ceilings keeping both the head and the tail with a marker between them. Exit codes are `0` for success, `1` for a capability that ran and failed, `2` for a syntax error or an exhausted limit, `124` for the wall-clock deadline, `126` for a denied capability, and `127` for an unknown command; `exit N` wraps as `N mod 256`. The command prints the script's combined output followed by an `[exit code: N]` line and exits with that code.

`curl` in this shell speaks no HTTP itself. It parses curl-style flags into the `{uri, method, headers, body}` shape and submits it to the single capability named by `--curl-capability`; without that flag it reports "command not found". Direct mode's linker is empty by design, so no HTTP-importing component loads there and `curl` cannot reach the network from the `shell` subcommand. To run a script whose `curl` actually resolves, use `prompt --broker`, where the same capability seam falls through to a broker that can authorize HTTP.

`gh` is the same idea with a GitHub-CLI-shaped vocabulary. It parses subcommands (`gh pr view 7 -R owner/repo`, `gh pr review 7 -R owner/repo --approve`, `gh content view <path> -R owner/repo`, `gh issue`, `gh repo`, `gh branch`, `gh commit`, `gh user`) and dispatches each one to the correspondingly named `gh.*` capability — `gh pr view` to `gh.pull-request.read`, `gh pr review --approve` to `gh.pull-request.approve` — through the same budgeted capability seam. A subcommand whose capability is not granted reports the missing capability by name with exit code `127`; flags that would change what a command means (`--json`, `--jq`, `--web`, `--checkout`) are rejected with guidance rather than ignored, and `gh api` is refused outright because raw passthrough would bypass per-capability authorization. Every `gh.*` capability also remains directly invocable as a command word (`gh.pull-request.read --owner o --repo r --number 7`); the builtin is ergonomics, not authority.

## Broker client mode

Broker mode never loads a component and has no provider authority. It opens one fresh Unix connection, validates an owner-only single-link socket and the configured server peer UID, sends one strict bounded protocol request, and closes the connection. Invocation payloads contain capability, caller-generated invocation/trace IDs, and JSON input—never principal, actor, policy, constraints, credentials, or authorization state. The server derives identity from peer credentials and chooses all authority.

Both connection flags are optional. `--socket` resolves in strict precedence order: the flag, then `$DEKOPON_BROKER_SOCKET`, then `$XDG_RUNTIME_DIR/dekopon/broker.sock`, then `$HOME/.local/run/dekopon/broker.sock`; if none apply the command fails with `could not determine broker socket path`. Candidate paths are never probed for existence, because a socket is legitimately absent while the daemon is stopped—the tightest resolved tier is trusted and a stopped daemon surfaces as a connect failure against that exact path. `--server-uid` defaults to the caller's own effective UID, which is correct for the common per-user broker sharing one owner-UID trust domain; pass it explicitly for a broker running under a dedicated service account. Neither default weakens validation: socket ownership and connected peer credentials are checked against the resolved values exactly as they are against explicit ones.

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
  jsonplaceholder.posts.get --input '{"postId":7}'
```

The caller must generate and retain unique invocation IDs; reuse is durably denied. The client never retries automatically: after a lost response to an external write, treat the outcome as unknown and consult broker audit rather than issuing a new ID blindly. A broker failure response distinguishes the two cases explicitly — `broker-unavailable` means no provider work began, while `outcome-unaudited` means the effect may already have happened and was not recorded, so it must not be resubmitted under any identifier. See the failure-code table in `broker-http.md`. `--max-frame-bytes` and `--io-timeout-ms` constrain client allocation and each connect/frame operation. Broker results are `InvocationResult` JSON with policy decision linkage and evidence. Provider output is intentionally printed to the invoking client but remains absent from broker audit fields. Direct Wasm limits do not appear in broker subcommands because only broker policy and host ceilings constrain provider execution.

## Prompt mode

Prompt mode supports either an OpenAI-compatible Chat Completions endpoint or a ChatGPT/Codex subscription. Both backends expose the same bounded tool loop.

A session offers the model exactly **one** tool, named `bash`, whose single `script` argument is a [`dekopon-shell`](../crates/dekopon-shell/) script. It is not one tool per capability: a model expresses a whole multi-step plan — loops, conditionals, JSON handling, several capability calls — in one script instead of being fed one capability per turn, and the tool surface stays a single fixed schema no matter how many capabilities an operator grants. The tool returns the script's combined output followed by an `[exit code: N]` trailer, byte-for-byte what `dekopon-run shell` prints, so any script a model wrote can be replayed by an operator unchanged.

The model discovers what it can reach from inside the script rather than from the schema: `cap --list` returns the granted capability IDs and `cap --describe <capability>` returns one capability's input schema. There is no `help` builtin; the tool description carries the dialect.

### OpenAI-compatible endpoints

The default endpoint is `http://127.0.0.1:11434/v1`, suitable for an Ollama-compatible local server:

```console
cargo run -p dekopon-run -- prompt \
  --provider examples/providers/echo-provider.wasm \
  --model qwen3 \
  'Upcase the word hello'
```

Model arguments remain untrusted JSON: a tool call must be a JSON object carrying a string `script`, and anything else ends the session rather than being guessed at. Capability inputs the script assembles are not schema-validated by the host, so each provider must still validate its own required fields, types, and operation-specific constraints. Script results are returned to the model until it emits final text or `--max-steps` is reached.

Two bounds constrain a session, and they bound different things. A single model turn is capped at four tool calls: one script already expresses a multi-step plan, so a correct turn calls the tool once, and this only catches a runaway endpoint. The bound that limits actual work is `--shell-max-capability-calls`, which in prompt mode is a **whole-session** ceiling rather than a per-script one — a later script receives whatever earlier scripts left, and exhausting it trips the interpreter's own documented limit. Without that, a model widens its own budget simply by writing another script, and `--max-steps` multiplies the ceiling instead of bounding it. Every other interpreter bound (`--shell-max-steps`, output bytes and lines, `--shell-timeout-ms`, value bytes) applies per script exactly as in shell mode. A script's variable namespace is still seeded only by its own assignments; prompt mode reads the process environment for its own configuration and never exposes it to a script.

### Reaching a broker from prompt mode

By default a prompt session is direct-only and contacts no broker, so a local demo or a CI run needs no daemon. Passing `--broker` additionally routes any capability the loaded components do not offer to a running `dekopon-brokerd`, using the same socket and UID defaulting as the `broker` subcommands (`--socket`, then `$DEKOPON_BROKER_SOCKET`, then `$XDG_RUNTIME_DIR/dekopon/broker.sock`, then `$HOME/.local/run/dekopon/broker.sock`).

This is what makes an HTTP-capable capability reachable at all. Direct mode's linker is import-free by construction, so a component there cannot perform I/O; `curl` and any fetching capability therefore resolve over the broker leg or not at all. Dispatch checks the direct registry first — a capability that can run locally always does, without an authorization decision or an audit record — and falls through to the broker only for what direct mode cannot serve. The broker remains the sole authority: `dekopon-run` submits an identity-free proposal per call and reports back whatever the broker decided, so a policy refusal surfaces to the script as exit code `126` (denied) rather than a generic failure, and a capability outside the session as `127`. Each call carries a freshly generated invocation ID extending one per-session trace ID, because the broker treats an invocation ID as a durable replay-rejection key.

Passing `--socket` or `--server-uid` without `--broker` is a usage error rather than a silent no-op:

```console
cargo run -p dekopon-run -- prompt \
  --provider examples/providers/echo-provider.wasm \
  --broker --server-uid "$(id -u)" \
  --curl-capability http-probe.fetch \
  --model qwen3 \
  'Fetch the service status page and summarize it'
```

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
  'Upcase the word hello'
```

Use an exact model exposed to the signed-in Codex account; `gpt-5.5` is a recovery choice when the account does not expose GPT-5.6. Dekopon automatically refreshes an expiring access token and replays opaque encrypted reasoning items only in memory when a tool call requires another model turn.

The default credential file is `~/.config/dekopon/chatgpt-auth.json` (`0600` on Unix). `DEKOPON_CHATGPT_AUTH_FILE`, `dekopon auth chatgpt ... --auth-file`, or `dekopon-run prompt --chatgpt-auth-file` can override it. Dekopon intentionally never imports OAuth material from pi, OpenClaw, or the Codex CLI. The model request is sent only to `auth.openai.com` during login and `chatgpt.com/backend-api/codex/responses` during inference; those endpoints are fixed rather than user-configurable.

The subscription transport receives the prompt, system instruction, the single scripting tool schema, the scripts the model writes, and their output. Credentials are never passed to Wasm providers or trace fields. Subscription quotas and model availability remain controlled by OpenAI and are distinct from Platform API billing.

## Rust provider interface

Provider source implements `dekopon_provider_sdk::Provider` and uses `export_provider!`. See [`../examples/providers/echo/src/lib.rs`](../examples/providers/echo/src/lib.rs).

The SDK adapter exposes [`../crates/dekopon-provider-sdk/wit/provider.wit`](../crates/dekopon-provider-sdk/wit/provider.wit):

```wit
world provider {
    export describe: func() -> string;
    export invoke: func(capability: string, input-json: string) -> string;
}
```

The strings carry strictly typed manifest and response JSON. This keeps the first WIT surface deliberately small while the Rust trait and wire model stabilize. The same world is distributed as the `dekopon:provider@0.1.0` WIT package for provider toolchains; the package has exactly these two exports and zero imports. Providers can use `export_provider_with_bindings!` to retain those exports in a caller-generated world with versioned imports. The checked-in HTTP probe demonstrates that composition, while direct mode deliberately rejects it because distribution and structural imports do not change the empty runtime linker or grant provider authority.

Build providers for `wasm32-unknown-unknown`, then componentize the embedded WIT metadata. The echo provider is a separate Cargo workspace, and its checked-in component is generated rather than hand-edited. The [echo-provider README](../examples/providers/echo/README.md) and [`development.md`](development.md) contain exact build and validation commands. A `wasm32-wasip2` build imports WASI and will be rejected because this host intentionally links no guest imports.

## Tracing, logs, and limits

`--trace <PATH>` writes Chrome/Perfetto-compatible JSON containing runner, model, component compilation, description, and invocation spans. An optional `--otlp-endpoint <URL>` (or `OTEL_EXPORTER_OTLP_ENDPOINT`) also exports correlated OTLP/HTTP protobuf traces and structured lifecycle logs. The URL is a generic base to which `/v1/traces` and `/v1/logs` are appended. Standard OTLP header environment variables carry receiver authentication and routing without exposing credentials in process arguments. The short-lived runner flushes configured exporters before returning, and a failed flush fails the command.

Prompt text, model responses, model-authored script text and its output, provider input/output, bearer tokens, OTLP authorization headers, and raw errors are intentionally excluded from telemetry. Lifecycle logs record stable command/session/model/script/guest events and share generated trace and span IDs with performance traces. They are operational audit telemetry, not broker authorization evidence or a replacement for durable broker audit. See [`observability.md`](observability.md) for configuration, event semantics, data minimization, and the single-container OpenObserve example.

Direct-operation bounds are configurable on `inspect`, `invoke`, and `prompt` with:

- `--max-memory-bytes`
- `--max-input-bytes`
- `--max-output-bytes`
- `--fuel`
- `--timeout-ms`

The host supplies no WASI, filesystem, network, environment, clock, random, or credential imports. It accepts only capabilities declaring `read-only`. Consequently this path exercises pure provider computation; it does not grant read access to an external system despite the effect label.

## Authority limitation

A model tool call in direct `dekopon-run` mode is not an `AuthorizedInvocation`. Immediate mode performs no broker transition and must not be extended to provider credentials, host networking, local writes, or external writes. Explicit broker mode submits proposals without receiving effect authority; the authenticated, policy-controlled, separately deployed broker owns HTTP imports and execution as described in [`broker-http.md`](broker-http.md), [`design.md`](design.md), and [`security-model.md`](security-model.md).
