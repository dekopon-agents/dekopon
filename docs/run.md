# Direct provider runner and broker client

`dekopon-run` is an **experimental current** one-shot runner for developing and measuring read-only import-free providers, plus explicit unprivileged clients for two separately running processes: `dekopon-brokerd` and `dekopond`. It is separate from the `dekopon` operator CLI and is not a daemon, policy engine, authorization broker, or production provider boundary.

## Commands

```text
dekopon-run inspect --provider <COMPONENT>...
dekopon-run invoke --provider <COMPONENT>... <CAPABILITY> [--input <JSON> | --input-file <PATH>] [--repeat <COUNT>]
dekopon-run prompt --provider <COMPONENT>... --model <MODEL> [--endpoint <URL> | --chatgpt-subscription] [--broker [--socket <PATH>] [--server-uid <UID>]] [--curl-capability <CAPABILITY>] [--system <TEXT>] [--skill <DIRECTORY>]... [--suggestions] <PROMPT>
dekopon-run shell --provider <COMPONENT>... [--curl-capability <CAPABILITY>] <SCRIPT>
dekopon-run broker capabilities [--socket <PATH>] [--server-uid <UID>]
dekopon-run broker invoke [--socket <PATH>] [--server-uid <UID>] --invocation-id <ID> --trace-id <ID> <CAPABILITY> [--input <JSON> | --input-file <PATH>]
dekopon-run session list [--openobserve-url <URL>] [--since <DURATION>] [--limit <COUNT>] [--json]
dekopon-run session show (--trace-id <TRACE_ID> | --from-file <PATH>) [--json]
dekopon-run session replay (--trace-id <TRACE_ID> | --from-file <PATH>) --model <MODEL> [--endpoint <URL> | --chatgpt-subscription] [--system <TEXT> | --system-file <PATH>] [--skill <DIRECTORY>]... [--suggestions] [--provider <COMPONENT>]... [--max-steps <COUNT>] [--json]
dekopon-run chat --gateway <SOCKET> --subject <SUBJECT> [--conversation <ID>]
dekopon auth chatgpt <login | status | logout | export>
```

Every `--provider <COMPONENT>` is a component file or a directory, which expands to the `*.wasm` files directly inside it in filename order, so two runs over one directory agree about which provider claimed a duplicate capability. `--input-file -` reads the capability input from standard input on both `invoke` and `broker invoke`.

**`prompt` and `chat` have different execution models, and the difference is the whole point of having both.** `prompt` runs the model and tool loop **in this process**: it compiles provider components, calls a model endpoint, and executes each script itself. `chat` runs **no loop at all**. It loads no component, contacts no model, and holds no provider authority; it writes a JSON line to a running [`dekopond`](dekopond.md)'s development socket and prints the line that comes back, while routing, attestation, authorization, and the model call all happen inside that daemon on exactly the path a Slack message takes.

Each direct `inspect`, `invoke`, `shell`, or `prompt` command (and a `session replay` given `--provider`) builds one `ProviderRegistry`, compiles every selected component once, and retains that machine code only for the registry's lifetime. `--compile-cache <DIRECTORY>` (or `DEKOPON_RUN_COMPILE_CACHE`) additionally points Wasmtime's content-addressed cache at a directory, so a later process reads compiled code back instead of running Cranelift again; without it every process recompiles every selected component. Description and invocation calls receive a fresh Wasmtime store and component instance with configured memory, table, instance, fuel, wall-clock, input, and output limits; one shared runtime mutex serializes component calls, and one long-lived worker thread arms each call's wall-clock deadline. Repeating `--provider` creates one deterministic capability registry, and duplicate provider IDs, duplicate capability IDs, and command-word conflicts fail before invocation — all of them in one report, the same conflicts `dekopon-brokerd` refuses to start with, so a provider that loads here also loads there. Success exits `0`, runtime/model/provider failures exit `1`, and Clap usage failures exit `2`. Broker invocations always print the typed result; `Denied` or `Failed` outcomes exit `1`, while `Succeeded` exits `0`.

The standalone Rust echo provider is immediately runnable after fetching its exact v0.1.0 release
fixture:

```console
ci/fetch-external-provider-components.sh examples/providers echo
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

`dekopon-run shell` runs one script through [`dekopon-shell`](../crates/dekopon-shell/), a sandboxed bash-flavored interpreter whose command words dispatch to provider capabilities instead of operating-system processes. It contacts no model. Provider loading and the synchronous interpreter execute on Tokio's blocking pool as one opaque, joined [`dekopon-process`](../crates/dekopon-process/) node. That node is explicitly non-interruptible after start: its own existing shell/provider deadlines still apply, and this lifecycle seam exposes no cancellation path that could return while blocking work continues. If its outer caller is dropped, the supervisor delivers the full result to the runner's abandonment observer while the Tokio runtime remains alive; normal runner command execution keeps that runtime alive, and runtime shutdown is the ownership boundary. Inside it, each provider command word a script runs is one nested non-interruptible `direct-command` node around the guest call; shell pipeline stages are not process nodes, so values, variable scope, output, and exit status are unchanged. The same interpreter backs the single `bash` tool prompt mode offers, so `shell` is the way to develop and replay a script by hand exactly as a model would run it.

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

The language keeps `if`/`elif`/`else`, `for`, `while`, `until`, `case`/`esac`, `[[ ... ]]`, and `{ ...; }` groups — usable as pipeline stages, so `cmd | while ...; do ...; done` and `cmd || { echo failed; exit 1; }` both work, and a piped loop keeps the variables it assigns because there is no subshell to lose them in — `break`/`continue` with levels, functions with `$1`/`$@`/`$*`/`$#`, `shift`, `getopts`, and `local`, `read` (which consumes one line per call, so `cmd | while read line; do ...; done` terminates), `&&`/`||`/`;`/`|`, a leading `!` to invert a pipeline, `$?`, `${PIPESTATUS[@]}`, `set -e`/`set -u`/`set -o pipefail` and their `+` forms, `$(( ))` arithmetic, parameter expansion (`${NAME:-w}`, `${NAME:=w}`, `${NAME:?w}`, `${NAME:+w}`, `${#NAME}`, `${NAME[@]}`, `${NAME#p}`, `${NAME%p}`, `${NAME/p/r}`), both quoting forms, here-documents (`<<EOF`, `<<-EOF`, and the literal `<<'EOF'`), and `#` comments. `${#NAME}` counts characters of a string but elements of an array and keys of an object, and the `#`/`%`/`/` patterns are literal text like every other pattern here.

Dropped constructs fall into two groups, and the difference matters:

- **Rejected loudly**, by name, as a parse or run failure: backticks (use `$( )`), job control (a trailing `&`), subshells, `(( ))`, `name=(a b c)`, C-style `for (( ))`, every `set` option this shell does not enforce (`-x`, `-o noclobber`, `--`), descriptors other than 1 and 2 (`3>`, `<&`), here-strings (`<<<`), `case` fall-through (`;&`, `;;&`), process substitution, `eval`, `exec`, `source`, `declare`, `export`, bash's array emulation, case-conversion and `@`-operator parameter expansions, and regex metacharacters in an unflagged `grep`/`sed` pattern or glob metacharacters in a `case` pattern.
- **Inert literals**, indistinguishable from ordinary text: globbing (`*`, `?`, `[abc]`), brace expansion (`{a,b}`), tilde expansion (`~`), and POSIX IFS word splitting. There is no filesystem to glob against and no `IFS` to split on, so there is nothing for these to be rejected against; an unquoted expansion holding a JSON array is what produces multiple words here.

Everything in the first group fails rather than doing something else, so a model can never believe something happened that did not. That rule reaches inside kept constructs too: a `case` pattern is literal text, so `*)` is still the default branch but `*.json)` is a parse error rather than a silent mismatch — the same treatment a regex metacharacter gets in an unflagged `grep` pattern, and for the same reason. `grep -E` and `sed -E` are the explicit way through, and the only two places regex syntax means regex syntax: they compile against `regex-bites`, the engine `jq` already links, report its compile error by name, and bound an `-E` pattern's source length, compiled size, and nesting before it sees input. A here-document's body arrives as one JSON string, since a block of literal text is a string in this value model; pipe it through `jq 'fromjson'` when it holds JSON, because a body that merely looks like JSON is never parsed behind the script's back.

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

The interpreter's variable namespace is seeded only by the script's own assignments; it never reads the host process environment, so `$PATH` and `$OPENAI_API_KEY` are unset inside a script. That covers `jq` too: jaq's `env` and `now` filters are not linked, so `jq -r env.SECRET` reports an undefined filter. `--shell-allow-clock` does not reopen either one — it grants the `date` builtin a wall-clock reading and nothing more, and `date` consults no environment variable, so `TZ` cannot be observed through it and the output is always UTC. Output is truncated to the configured ceilings keeping both the head and the tail with a marker between them. Exit codes are `0` for success, `1` for a capability that ran and failed or a provider command word whose run never reached the provider's answer, `2` for a syntax error, an exhausted limit, or a command word's usage error, `124` for the wall-clock deadline, `126` for a denied capability or a command word cancelled underneath a gateway session, and `127` for an unknown command; `exit N` wraps as `N mod 256`. The command prints the script's combined output followed by an `[exit code: N]` line and exits with that code.

`curl` in this shell speaks no HTTP itself. It parses curl-style flags into the `{uri, method, headers, body}` shape and submits it to the single capability named by `--curl-capability`; without that flag it reports "command not found". A broker-backed call may use exact `--oauth2-bearer '${drn:…}'` or `-u 'USER:${drn:…}'` forms. The reference travels beside provider input as inert typed proposal data and direct mode refuses it; see [`secrets.md`](secrets.md). Direct mode's linker is empty by design, so no HTTP-importing component loads there and `curl` cannot reach the network from the `shell` subcommand. To run a script whose `curl` actually resolves, use `prompt --broker`, where the same capability seam falls through to a broker that can authorize HTTP and, independently, exact DRN use.

A **provider** can contribute command words the same way, and one that does no longer needs to be in this repository to do it. A component declaring `commandWords` in its manifest and exporting `run-command` (or the legacy `resolve-command`) turns its own argv into a capability proposal — `gh pr view 7 -R owner/repo` into `gh.pull-request.read`, say — and the result then travels the identical path a direct capability word takes: same budget, same denial, same telemetry. The rewrite proposes; it does not grant. A `run-command` guest may instead answer with rendered text — a help page, a usage error — and may read the value piped into the word, so a word behaves like the command-line tool it fronts: `gh --help` prints its page on stdout at exit `0`, `gh bogus` prints a usage error on the diagnostic stream at exit `2`, and `echo hello | probe upper -` hands the piped value to the provider under its own convention for asking (a trailing `-`, for the shipped fixtures). Rendered text obeys the shell's value and output ceilings and charges no capability call. A run that never reached the provider's answer is neither a proposal nor a usage error: a host refusal or a broker transport failure exits `1` naming its cause, and a run cancelled underneath a gateway session exits `126`. A word whose capability is not granted in this session reports the missing capability by name with exit code `127`, and every capability stays directly invocable regardless (`gh.pull-request.read --owner o --repo r --number 7`), so the vocabulary is ergonomics and never authority. Because the table is compiled beside the capability list it maps onto, a capability renamed and forgotten is a build error in the provider rather than an exit code `127` a model discovers mid-session. The GitHub vocabulary that used to be a shell builtin here is now exactly this: [`dekopon-provider-gh`](https://github.com/dekopon-agents/dekopon-provider-gh) declares the word `gh` and owns its own subcommand table.

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

The caller must generate and retain unique invocation IDs; reuse is durably denied. The client never retries automatically: after a lost response to an external write, treat the outcome as unknown and consult broker audit rather than issuing a new ID blindly. A broker failure response distinguishes the two cases explicitly — `broker-unavailable` means no provider work began, while `outcome-unaudited` means the effect may already have happened and was not recorded, so it must not be resubmitted under any identifier. A client-side framing failure preserves the same distinction: `ClientError` records whether the request or the response half failed, and a response-phase failure reaches a prompt script as `denied` (exit `126`) so a model cannot read it as a retryable error. See the failure-code table in `broker-http.md`. `--max-frame-bytes` and `--io-timeout-ms` constrain client allocation and each connect/frame operation. Broker results are `InvocationResult` JSON with policy decision linkage and evidence. Provider output is intentionally printed to the invoking client but remains absent from broker audit fields. Direct Wasm limits do not appear in broker subcommands because only broker policy and host ceilings constrain provider execution.

## Gateway chat client

`dekopon-run chat` holds an ongoing conversation with a running `dekopond` over its [local development transport](dekopond.md#local-development-transport). It is a socket client and nothing more: no Wasm component is loaded, no model is contacted, no script is executed, and no provider authority is held or requested. Each line of standard input becomes one `{"subject", "channel", "text"}` request, and the single `{"reply": "..."}` line that comes back is printed. That makes it the interactive counterpart of the `nc -U` session in [`dekopond.md`](dekopond.md), and it is the reason `chat` sits beside the direct provider path without touching the read-only, import-free rule that path lives by.

```console
dekopon-run chat \
  --gateway "$HOME/.local/run/dekopon/dekopond-dev.sock" \
  --subject tel.16034700182 \
  --conversation morning-standup
```

- `--gateway <SOCKET>` is the path the gateway's `local` transport binds. There is no default and no discovery order: the path comes from the daemon's own configuration, and guessing one would connect to whatever happened to be there.
- `--subject <SUBJECT>` is the canonical external subject the session claims, such as `tel.16034700182`, `discord.123456789012345678`, or `slack.t0123abc.u9xyz`. It is parsed here, so a non-canonical value is a usage error exiting `2` rather than a line the gateway discards without answering — which would look like an unresponsive daemon.
- `--conversation <ID>` is sent as the `channel` of **every** request in the session, and is the conversation's identity. Omitted, one is minted and announced on standard error as `conversation: chat-<hex>`, so the session can be resumed by passing that value back. It is announced on standard error rather than standard output because standard output is the reply stream and nothing else. The identifier is caller-chosen by design: a process identifier would be the wrong choice, because PIDs recycle and every invocation is a new process, so nothing derived from one survives to be resumed.

Piped standard input is the same loop, so non-interactive use needs no separate mode:

```console
printf 'what changed today?\nand what did it cost?\n' \
  | dekopon-run chat --gateway "$socket" --subject tel.16034700182 --conversation audit-2026-08-17
```

**Exactly one message is ever in flight.** The local protocol carries no correlation identifier, so a reply is matched to its request by ordering alone; this client therefore never pipelines and waits for each answer before reading the next line. A blank input line asks nothing and is skipped rather than sent. A message that would not fit the transport's 64 KiB line — counting the JSON envelope and the newline the gateway delimits by — is refused here with exit code `1`, because the transport's own reaction to an over-long line is to close the connection without a diagnostic, which would reach the operator as an unexplained hang-up.

Exit codes follow the rest of the runner: `0` when standard input ends normally, `2` for a usage failure, and `1` for every session failure — the gateway closing the connection, a line from the socket that is not a reply, an over-long message, or a socket that will not connect. Replies already printed are kept; only the unanswered request fails the session. A consumer that stops reading (`| head -1`) ends the session cleanly rather than failing it.

`--conversation` now participates in both optional memory mechanisms. A `persistent` route replays
its bounded process-memory window immediately. Separately, an agent with the complete broker-owned
memory surface may run `memory recent --last N` or `memory search --query TEXT` to retrieve durable
turns across restarts. Durable text is never replayed automatically, and `memory.chat.record` is
absent from shell listing, description, command resolution, and generic invocation.

**This is not a stateless dev socket.** The local transport sends `--conversation` as the message's
`channel`, which the gateway takes verbatim as the conversation identity, and history is keyed on
`(transport, conversation identity, the sender's canonical subject)`. So on a `persistent` route the
pair `--subject` and `--conversation` names an existing history rather than opening a fresh one, and
because the local transport trusts its caller to declare a subject, naming one a Slack sender
created replays that person's compacted exchange into this prompt. No authority moves — the broker
decides every invocation for itself — but text does. That is a second reason the socket is `0600`
and a development tool. See [`dekopond.md`](dekopond.md#conversations) for the window's bounds,
compaction, and grant-change invalidation.

The identifier's other effect is on the gateway's admission check, which keys a separate in-flight set on `(transport, channel, thread)` with no subject in it. Do not run two sessions on one conversation identifier at once: the second message is refused as busy, which arrives as the reply `I'm busy — try again shortly.` unless the gateway's `replyOnBusy` is turned off, in which case the refusal is silent and this client waits for a reply that never comes. A minted identifier is unique per invocation, so reaching that requires passing the same `--conversation` to two concurrent sessions deliberately.

Nothing about this command widens what a caller can reach. The local transport trusts its caller to declare a subject — that is what makes it a development transport rather than a production one — and it grants nothing by doing so, because the declared subject is only a claim the broker must still map. The broker needs an attestor grant covering that namespace plus an owner-controlled mapping before the claim resolves to a principal, so a session reaches exactly the authority the owner already configured for the subject it names. The socket's `0600` mode keeps it reachable only by the owner's UID. See [`dekopond.md`](dekopond.md#local-development-transport) for the transport's side of that position.

A chat session appears in traces as a single `runner.chat` span carrying no fields. The socket path, the declared subject, the conversation identifier, and every message and reply are all excluded, consistent with the rest of the runner's telemetry.

## Prompt mode

Prompt mode supports either an OpenAI-compatible Chat Completions endpoint or a ChatGPT/Codex subscription. Both backends expose the same bounded tool loop.

By default a session offers the model exactly **one** tool, named `bash`, whose single `script` argument is a [`dekopon-shell`](../crates/dekopon-shell/) script. It is not one tool per capability: a model expresses a whole multi-step plan — loops, conditionals, JSON handling, several capability calls — in one script instead of being fed one capability per turn, and the tool surface stays a single fixed schema no matter how many capabilities an operator grants. The tool returns the script's combined output followed by an `[exit code: N]` trailer, byte-for-byte what `dekopon-run shell` prints, so any script a model wrote can be replayed by an operator unchanged.

Before request one the harness supplies sorted fresh descriptions and complete input schemas from the same scoped snapshot as `cap --list`/`cap --describe`; those commands remain fallback inspection, not a required discovery turn. Metadata over 256 capabilities/128 KiB is refused without truncating schemas. There is no `help` builtin; the tool description carries the dialect.

The shared prompt loop can accept additional embedder-owned tools. `dekopond` supplies bounded chat-asset and credential-free `inspect_agent_config` tools, plus one-attempt `generate_image` only on an explicitly configured route; `dekopon-run prompt` supplies none of them; beyond `bash` it offers only the two opt-in tools below, [`read_skill`](#mounting-skills) and [`suggest_improvement`](#improvement-suggestions), which a gateway session reaches through its route instead. Generated bytes leave through a gateway-owned output slot and never become a model message or runner output.

The shared runtime is now [`dekopon-harness`](harness.md), with mandatory accounting and bounded
process-local checkpoints. Direct and replay modes never offer model/effort controls, even with a
provider broker leg. Memory receipts do not imply crash durability; stdout delivery currently
finalizes as unknown rather than treating returned output as a transport receipt.

### OpenAI-compatible endpoints

The default endpoint is `http://127.0.0.1:11434/v1`, suitable for an Ollama-compatible local server:

```console
cargo run -p dekopon-run -- prompt \
  --provider examples/providers/echo-provider.wasm \
  --model qwen3 \
  'Upcase the word hello'
```

Model arguments remain untrusted JSON: a tool call must be a JSON object carrying a string `script`, and anything else ends the session rather than being guessed at. Capability inputs the script assembles are not schema-validated by the host, so each provider must still validate its own required fields, types, and operation-specific constraints. Script results are returned to the model until it emits final text or `--max-steps` is reached.

Two bounds constrain a session, and they bound different things. A single model turn is capped at ten tool calls: one script already expresses a multi-step plan, while gateway-owned meta tools can reasonably fan out over several attachments, and this ceiling only catches a runaway endpoint. The bound that limits capability work is `--shell-max-capability-calls`, which in prompt mode is a **whole-session** ceiling rather than a per-script one — a later script receives whatever earlier scripts left, and exhausting it trips the interpreter's own documented limit. Embedder-owned meta tools keep their own work ceilings; for example, the gateway still opens at most four chat attachments per session. Without that, a model widens its own budget simply by writing another script, and `--max-steps` multiplies the ceiling instead of bounding it. Every other interpreter bound (`--shell-max-steps`, output bytes and lines, `--shell-timeout-ms`, value bytes) applies per script exactly as in shell mode. A script's variable namespace is still seeded only by its own assignments; prompt mode reads the process environment for its own configuration and never exposes it to a script.

### Reaching a broker from prompt mode

By default a prompt session is direct-only and contacts no broker, so a local demo or a CI run needs no daemon. Passing `--broker` additionally routes any capability the loaded components do not offer to a running `dekopon-brokerd`, using the same socket and UID defaulting as the `broker` subcommands (`--socket`, then `$DEKOPON_BROKER_SOCKET`, then `$XDG_RUNTIME_DIR/dekopon/broker.sock`, then `$HOME/.local/run/dekopon/broker.sock`).

This is what makes an HTTP-capable capability reachable at all. Direct mode's linker is import-free by construction, so a component there cannot perform I/O; `curl` and any fetching capability therefore resolve over the broker leg or not at all. Dispatch checks the direct registry first — a capability that can run locally always does, without an authorization decision or an audit record — and falls through to the broker only for what direct mode cannot serve. The broker remains the sole authority: `dekopon-run` submits an identity-free proposal per call and reports back whatever the broker decided, so a policy refusal surfaces to the script as exit code `126` (denied) rather than a generic failure, and a capability outside the session as `127`. Each call carries a freshly generated invocation ID extending one per-session trace ID, because the broker treats an invocation ID as a durable replay-rejection key. A provider command word takes the same two-leg path: a word a loaded component declares runs locally in a nested `direct-command` node, and a word only the broker's providers declare travels as `runCommand` inside a `broker-command` node — cancellable in contract, never cancelled here, because only an embedder such as `dekopond` supplies a signal; a runner session runs each word to the broker's answer or the transport's failure.

Passing any broker connection flag — `--socket`, `--server-uid`, `--max-frame-bytes`, or
`--io-timeout-ms` — without `--broker` is refused before any model call rather than silently
ignored: the command prints
`error: broker connection flags were supplied without --broker; a prompt session contacts no broker until you ask it to`
and exits `1`:

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

Dekopon refreshes an expiring access token by rotating the refresh token and writing the replacement back, so the credential file's directory has to be writable. [`chatgpt-credential.md`](chatgpt-credential.md) follows what that means for a container, including `dekopon auth chatgpt export`.

The default credential file is `~/.config/dekopon/chatgpt-auth.json` (`0600` on Unix). `DEKOPON_CHATGPT_AUTH_FILE`, `dekopon auth chatgpt ... --auth-file`, or `dekopon-run prompt --chatgpt-auth-file` can override it. Dekopon intentionally never imports OAuth material from pi, OpenClaw, or the Codex CLI. The model request is sent only to `auth.openai.com` during login and `chatgpt.com/backend-api/codex/responses` during inference; those endpoints are fixed rather than user-configurable.

The subscription transport receives the prompt, system instruction, the single scripting tool schema, the scripts the model writes, and their output. Credentials are never passed to Wasm providers or trace fields. Subscription quotas and model availability remain controlled by OpenAI and are distinct from Platform API billing.

[`inference.md`](inference.md) shows the exact Rust request types and pre-wire Responses JSON, then separates Dekopon's cache-friendly request shape from the public OpenAI API's caching documentation. OpenAI publishes no cache-retention contract for the private ChatGPT subscription endpoint, so only provider-reported cached-token usage establishes an observed hit.

### Mounting skills

`--skill <DIRECTORY>` mounts one skill and repeats for several. A skill is a directory named after the skill, holding a `SKILL.md` that opens with YAML front matter — `name`, which must equal the directory name, and a one-line `description` — followed by Markdown instructions; every other regular file under the directory is a *resource* of the skill, addressed by its `/`-separated relative path. This is the Agent Skills directory format, so a `SKILL.md` written for another agent loads here unchanged; [`examples/local/skills/pull-request-review`](../examples/local/skills/pull-request-review/SKILL.md) is one, with a `references/risk-checklist.md` resource. Everything is read into memory before the session starts, bounded (a 64 KiB `SKILL.md`, 256 KiB per resource, 64 resources, 1 MiB in all), so the session itself never opens a file. A directory that does not load — no `SKILL.md`, a `name` that differs from the directory, an unknown front-matter key, a symlink anywhere in the tree, a file past its bound — fails the command before any model call with `error: a --skill directory could not be mounted` and exit `1`; `-v` prints the cause. Two mounted skills with one name are refused the same way (`was mounted twice`), because the model could not tell them apart.

What the model sees is deliberately less than what was loaded. A second system message follows the standing instructions: it begins `Skills mounted for this agent`, lists each skill as `- name: description`, and tells the model to call `read_skill` before doing work a skill covers. Bodies and resources stay out of the prompt, and the listing is deterministic for one mounted set, so it does not disturb a cached prompt prefix. The session offers `read_skill` beside `bash`, with `name` (required) and `resource` (optional, a resource path). Without `resource` it returns the skill's instructions framed with its name, description, and the list of its resource files; with one, that file's text. A second read of the same instructions or resource in one session is answered with a one-line pointer at the earlier result rather than a second copy, because a tool result stays in the conversation and is re-sent on every later turn. An unknown skill name or resource path is a refusal the model reads — naming the mounted skills, or the skill's resource files — and the session continues; malformed arguments (not an object, no `name`, an unexpected key) end the session exactly as a malformed `bash` call does. Each read fires `agent.skill.read` with the skill name, resource path, byte count, and whether it repeated an earlier one; each refusal fires `agent.skill.refused` with its reason (`unknown-skill` or `unknown-resource`). Both fire in either payload mode and carry operator-authored names only; the text reaches telemetry solely through the transcript events payload mode adds. Without `--skill` there is no listing and no tool. A skill is operator-authored text handed to the model, exactly as `--system` is: it grants no authority, it is readable by the model in full, and nothing secret belongs in one. A `dekopond` route mounts the skills its agent's catalog entry lists on every session.

### Improvement suggestions

`--suggestions` offers a further tool, `suggest_improvement`, through which the model tells the operator how the agent could be improved: a standing instruction that was wrong or missing, a skill that would have helped or that misled it, a capability it needed and was not granted, a limit it ran into, a tool that behaved differently from its description. The tool's own description tells the model to call it after the task is done or when genuinely blocked, at most three times, never instead of answering, and that the note goes to the operator's telemetry rather than to the person it is talking with. A call is a JSON object of six strings: `category` (`instructions`, `skill`, `capability`, `tool`, `limits`, or `other`), `target` (at most 128 bytes), `summary` (512), `evidence` (2048), `proposal` (2048), and `confidence` (`low`, `medium`, or `high`). A recorded one is answered `Recorded suggestion N of 3 for the operator.`; a well-formed call that fails a bound is answered with the bound (`Suggestion not recorded: …`) and the session continues, as does a fourth in one session; malformed arguments end the session as they do for every tool.

Each recorded suggestion fires `agent.improvement.suggested` carrying all six fields, and each refusal `agent.improvement.refused` with its reason (`invalid-category`, `invalid-confidence`, `empty-field`, `field-too-long`, `session-limit`). Both fire in **either** payload mode: the flag is off by default precisely because the record carries model-authored text, and passing it is the consent. The runner also prints what was recorded to standard error after the answer, so standard output stays the model's text alone:

```text
suggestion 1/1 [capability, high confidence] gh.pull-request.read: The read capability was not granted.
  evidence: exit code 127 on every attempt
  proposal: Grant gh.pull-request.read to this agent.
```

A line break inside `evidence` or `proposal` is indented to sit under its label. A suggestion changes nothing — no instruction, skill, limit, or grant moves because a model asked — and without the flag the tool is not offered. A `dekopond` route opts in with `improvementSuggestions: true` and leaves the records to telemetry; reading them back from OpenObserve is `SELECT * FROM "dekopon" WHERE audit_event = 'agent.improvement.suggested'`. [Session replay](#session-replay-and-evaluation) is how a proposal is checked before it ships.

## Session replay and evaluation

`dekopon-run session` reads sessions back from the OpenObserve stream the runner and the gateway export to, and replays one against a model with an operator's change applied. The three subcommands contact no broker and hold no provider authority; only `replay` runs a model, only a `replay` given `--provider` loads a component, and the scripts that model writes are answered from the recording rather than executed. They close a loop that is deliberately operator-driven: run sessions, `list` them, `show` a bad one, edit the instructions or write a skill, `replay` with `--system-file` and `--skill` and compare, then commit the change to the catalog. There is no store beyond the telemetry backend, no automatic prompt rewriting, and no grader; every artifact is either in the catalog or in the records.

The source is the transcript [`observability.md`](observability.md#model-and-tool-transcript) defines. `accounting.model.call` fires in either payload mode, so `list` covers every session a deployment ran; the transcript events (`agent.model.prompt`, `agent.model.answer`, `agent.tool.script`, `agent.tool.output`) exist only for sessions that ran with payload telemetry on — `--otel-telemetry-payloads true` on this runner, `telemetryPayloads` on the gateway — and `show` or `replay` of one recorded without them fails with `has N accounted model turn(s) but no transcript`, naming the trace.

### Reaching the receiver

`list`, and `show` or `replay` given `--trace-id`, query OpenObserve's search endpoint (`POST <base>/_search?type=logs`) with the same flags:

- `--openobserve-url <URL>` (or `DEKOPON_OPENOBSERVE_URL`): the organization base, such as `http://127.0.0.1:5080/api/default` — the same base the OTLP exporter posts to, so one deployment's `OTEL_EXPORTER_OTLP_ENDPOINT` is also its query base. It must carry no query, fragment, or `user:password@` userinfo. Missing, the command fails with `no OpenObserve URL; pass --openobserve-url or set DEKOPON_OPENOBSERVE_URL`.
- `--openobserve-stream <STREAM>` (or `DEKOPON_OPENOBSERVE_STREAM`; default `dekopon`): the log stream the exporters wrote to; letters, digits, and underscores only.
- `--openobserve-auth-env <NAME>` (default `DEKOPON_OPENOBSERVE_AUTHORIZATION`): the **name** of the environment variable holding the complete `Authorization` header value. The value never appears in an argument, and its absence is the failure: `environment variable DEKOPON_OPENOBSERVE_AUTHORIZATION is not set; it must hold the OpenObserve Authorization header value`.
- `--openobserve-timeout-ms <MILLISECONDS>` (default `10000`): the whole-request deadline for each search page.
- `--since <DURATION>` (default `7d`): how far back to look — a count followed by `s`, `m`, `h`, or `d`; zero is refused. For `--trace-id` it is the window the trace is searched in.

The client follows no redirects and uses no ambient proxy, so the credential cannot be forwarded to a host nobody named. A search reads pages of 500 records and follows at most 20, then warns on standard error to `narrow --since to see the rest`; each response is read to at most 32 MiB. A trace identifier is interpolated into the search SQL, so it is checked first: 1-128 characters from letters, digits, `-`, `_`, and `.`. OpenObserve stores the `audit.event` attribute as `audit_event`, folding every character outside letters, digits, and underscores; the reader accepts both spellings.

### `session list`

`dekopon-run session list [--limit <COUNT>] [--json]` groups every `accounting.model.call` record in the window by `trace_id` and prints the newest sessions first, at most `--limit` (default `50`):

```text
TRACE                             STARTED               TURNS    TOKENS  OUTCOME    SERVICE
4bf92f3577b34da6a3ce929d0e0e4736  2026-08-31T09:15:02Z      2        41  answered   dekopon-run
7c1e9a0b2d3f4e5a6b7c8d9e0f1a2b3c  2026-08-30T17:04:40Z      3         -  failed     dekopond
```

`STARTED` is the earliest accounted turn, RFC 3339 UTC to the second; `TURNS` the highest turn accounted; `TOKENS` the sum only when every accounted call reports `usage.total_tokens` and addition fits, otherwise `-`; `OUTCOME` is `failed` when any turn was accounted as failed, otherwise `answered` or `no-answer` by whether the last turn carried an answer; `SERVICE` is the records' `service_name`, or `-`. An empty window prints `(no sessions in the window)`. `--json` prints the same rows as an array of `{traceId, service, startedUs, endedUs, modelTurns, totalTokens, failed, answered}`, with microsecond epoch timestamps and `null` where nothing was reported.

### `session show`

`dekopon-run session show (--trace-id <TRACE_ID> | --from-file <PATH>) [--json]` reconstructs one transcript. Exactly one source is required; naming both or neither is a usage error. `--trace-id` fetches every record of that trace from the receiver; `--from-file` reads a transcript an earlier `session show --json` printed and touches no backend, so the receiver flags are not needed. The message vector is rebuilt from the first turn's `full` prompt plus each later turn's `delta`, the final answer from its own `agent.model.answer` record, each tool result by call identifier, and usage and duration per turn from the accounting records. Records may arrive in any order; a transcript that is not the shape the loop writes is refused as malformed, naming the event.

The text rendering shows `trace:`, each leading `system:` message, the exchanges a persistent route replayed as `user (earlier):` and `assistant (earlier):`, the `user:` prompt, then `turn N:` for each model turn — with `[<ms> ms, <tokens> tokens]` when the accounting record carried them — holding its `assistant:` text and every `script:` with its `output:` (`(not recorded)` when the session ended before answering it; a call that was not a script shows as `tool <name>:` with its arguments), and finally `answer:`, or `answer: (none recorded)`. `--json` prints the exact document `replay --from-file` reads back:

```json
{
  "traceId": "4bf92f3577b34da6a3ce929d0e0e4736",
  "system": ["Be brief."],
  "history": [],
  "prompt": "Upcase hello",
  "turns": [
    {
      "turn": 1,
      "toolCalls": [
        {
          "id": "call-1",
          "name": "bash",
          "arguments": "{\"script\":\"echo.upcase --message hello | jq -r .message\"}",
          "result": "HELLO\n[exit code: 0]"
        }
      ],
      "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 },
      "durationMs": 12.0
    },
    {
      "turn": 2,
      "content": "The script printed HELLO.",
      "toolCalls": [],
      "usage": { "inputTokens": 20, "outputTokens": 6, "totalTokens": 26 },
      "durationMs": 3.0
    }
  ],
  "answer": "The script printed HELLO."
}
```

`history` lists `{user, answer}` exchanges oldest first; `usage` may also carry `cachedInputTokens` and `reasoningOutputTokens`; a field that was not recorded is omitted. The file is the recording, so it can be kept, edited by hand, and replayed with no backend in the loop.

### `session replay`

```text
dekopon-run session replay (--trace-id <TRACE_ID> | --from-file <PATH>) \
  --model <MODEL> [--chatgpt-subscription [--chatgpt-auth-file <PATH>] | --endpoint <URL> --api-key-env <NAME>] [--model-timeout-ms <MILLISECONDS>] \
  [--system <TEXT> | --system-file <PATH>] [--skill <DIRECTORY>]... [--suggestions] \
  [--provider <COMPONENT>]... [--compile-cache <DIRECTORY>] [--max-steps <COUNT>] [--json]
```

Replay puts the recorded conversation to a model again — the recorded system messages unless replaced, the earlier exchanges, then the prompt — and answers every script the model writes from the recording, so **no capability runs and no effect happens** by default. The prompt is real, the scripts the recorded model wrote are known, and every output was recorded, which makes a recording the cheapest evaluation of a changed instruction, a new skill, or a different model. The model flags are `prompt`'s: `--model`, `--chatgpt-subscription` with `--chatgpt-auth-file`, or `--endpoint` (default `http://127.0.0.1:11434/v1`) with `--api-key-env` (default `OPENAI_API_KEY`), and `--model-timeout-ms` (default `120000`). `--max-steps` (default `8`) bounds the replayed session's model turns, the final answer included.

- `--system <TEXT>` or `--system-file <PATH>` (one or the other) replaces **every** recorded system message with these standing instructions; absent, the recorded ones are replayed. The file is read whole, must be UTF-8, and is bounded at 64 MiB — as is a `--from-file` transcript.
- `--skill <DIRECTORY>` mounts skills exactly as in `prompt` and drops any `Skills mounted for this agent` listing the recording carried, so the replay lists exactly the skills the model can read. Without it, a recorded listing is replayed as text like every other system message, but no `read_skill` tool is offered; mount the skill again to let the model read it.
- `--suggestions` offers `suggest_improvement` to the replayed model; what it records is printed to standard error as in `prompt` and carried in the report.
- `--provider <COMPONENT>` (repeatable; a file, or a directory of `*.wasm`) supplies read-only, import-free components that run a script the recording cannot answer, in direct mode under the same `--compile-cache` (or `DEKOPON_RUN_COMPILE_CACHE`), Wasm bounds (`--max-memory-bytes`, `--max-input-bytes`, `--max-output-bytes`, `--fuel`, `--timeout-ms`), and interpreter bounds (the `--shell-*` flags) as `prompt`. Nothing is loaded unless the flag is passed, so the default replay is provably effect-free; with it, a diverging script can compute but never reach the network, and `curl` has no capability to assemble for. Their command words are served exactly as `prompt` serves them, so a diverging script that runs `probe --help` renders the live component's page. `--shell-max-capability-calls` bounds the whole replayed session as it does in `prompt`; a script answered from the recording spends none of it.

A script is answered from the recording when its text exactly matches a recorded script whose output was recorded and not yet consumed, wherever that script sits, so a replayed model that reorders two independent scripts stays on the recorded trajectory, and the tool result is byte-for-byte what was recorded. The first script the recording cannot answer is the **divergence**, and what replay honestly cannot do is invent tool output for it:

- Without `--provider`, the script is answered with `[replay stopped: the recorded session never ran this script and no live providers were supplied to run it]` and the session ends there. The report's `divergence.handling` is `stopped`, the replayed answer is absent, and the exit code is `0`: stopping at a divergence is the replay doing its job, and the turns before it are a faithful comparison.
- With `--provider`, the script runs live and the session continues; `handling` is `live`. Later scripts are still answered from the recording when they match and run live when they do not, and only the first divergence is reported. From that point the report describes a new session, not a comparison.

The report (`--json`) is `{traceId, recorded, replayed, divergence, suggestions, error}`. `recorded` and `replayed` are each `{modelTurns, scripts, answer, usage}` — the scripts in order, the final answer or `null`, and unknown-aware usage from independent accounting calls, including failures and images. Legacy
files without `calls` fall back to their recorded turns; empty/missing observations are not zero. `divergence` is `null` or `{turn, script, unusedRecordedScripts, handling}`: the replayed turn that wrote the script, the script, and the recorded scripts not yet consumed. `suggestions` lists what `--suggestions` recorded. `error` is `null` unless the replayed session failed for a reason other than a divergence stop — a model failure, `--max-steps` exhausted, a malformed tool call — in which case the report is still printed and the exit code is `1`. The text rendering summarizes both sessions, states `divergence: none` or `divergence: turn N (stopped there | ran live), K recorded script(s) unused` with the script, compares scripts index by index as `script N (same | differs | recorded only | replayed only):` — the recorded text, and the replayed text when it is not the same — and ends with `answer (recorded):` and `answer (replayed):`:

```text
trace: 4bf92f3577b34da6a3ce929d0e0e4736
recorded: 2 turn(s), 1 script(s), 41 token(s), answer: yes
replayed: 1 turn(s), 1 script(s), 18 token(s), answer: no
divergence: turn 1 (stopped there), 1 recorded script(s) unused
  script:
    echo.downcase --message HELLO
script 1 (differs):
  recorded:
    echo.upcase --message hello | jq -r .message
  replayed:
    echo.downcase --message HELLO
answer (recorded):
    The script printed HELLO.
answer (replayed): (none)
```

Failures before a model is reached — an unreadable or oversized file, a `--from-file` that is not a transcript `session show --json` printed, a missing receiver URL or credential, a trace with no records or no transcript, a `--skill` that does not mount, a `--provider` that does not load — exit `1` with no report; usage errors exit `2`. In traces the `runner.command` root span names these commands `session.list`, `session.show`, and `session.replay`, and the replay span records the model identifier and backend, the provider count, the skill count, whether suggestions were offered, and whether the system prompt was replaced — not its text.

The loop end to end, with the receiver named by environment and the credential read by name:

```console
export DEKOPON_OPENOBSERVE_URL=http://127.0.0.1:5080/api/default
# DEKOPON_OPENOBSERVE_AUTHORIZATION holds the Authorization header value.
dekopon-run session list --since 24h

dekopon-run session show --trace-id 4bf92f3577b34da6a3ce929d0e0e4736 --json > session.json

dekopon-run session replay --from-file session.json \
  --model "$MODEL" \
  --system-file instructions.md \
  --skill examples/local/skills/pull-request-review

dekopon-run session replay --trace-id 4bf92f3577b34da6a3ce929d0e0e4736 \
  --model "$MODEL" \
  --provider examples/providers/echo-provider.wasm \
  --json
```

## Rust provider interface

Provider source implements `dekopon_provider_sdk::Provider` and uses `export_provider!`. The echo implementation and its release gates live in [`dekopon-provider-echo`](https://github.com/dekopon-agents/dekopon-provider-echo).

The SDK adapter exposes [`../crates/dekopon-provider-sdk/wit/provider.wit`](../crates/dekopon-provider-sdk/wit/provider.wit):

```wit
world provider {
    export describe: func() -> string;
    export invoke: func(capability: string, input-json: string) -> string;
}
```

The strings carry strictly typed manifest and response JSON. This keeps the first WIT surface deliberately small while the Rust trait and wire model stabilize. The same world is distributed as the `dekopon:provider@0.3.0` WIT package for provider toolchains; the `provider` world has exactly these two exports and zero imports. A second `provider-cli` world adds one export, `run-command` — argv plus an optional piped value, answered with a proposal, rendered text with an exit status, or a decline — for providers that contribute command words to the sandboxed shell, and a third `provider-commands` world keeps the legacy `resolve-command` rewrite. They are separate worlds rather than extra exports on the first so a host can require the base contract and look the command export up by name, which is what lets a component built against `0.1.0` or `0.2.0` keep loading; a host that finds both calls `run-command`. Providers can use `export_provider_with_bindings!` to retain those exports in a caller-generated world with versioned imports. The checked-in HTTP probe demonstrates that composition, while direct mode deliberately rejects it because distribution and structural imports do not change the empty runtime linker or grant provider authority.

Build providers for `wasm32-unknown-unknown`, then componentize the embedded WIT metadata. Echo is independently built, tested, and released by its standalone repository; core pins and fetches its exact v0.1.0 release checksum rather than tracking source or Wasm. [`development.md`](development.md) describes fetched external fixtures and the remaining in-tree conformance workspaces. A `wasm32-wasip2` build imports WASI and will be rejected because this host intentionally links no guest imports.

## Tracing, logs, and limits

`--trace <PATH>` writes Chrome/Perfetto-compatible JSON containing runner, model, component compilation, description, and invocation spans. An optional `--otlp-endpoint <URL>` (or `OTEL_EXPORTER_OTLP_ENDPOINT`) also exports correlated OTLP protobuf traces and structured lifecycle logs — over HTTP by default, where the URL is a generic base to which `/v1/traces` and `/v1/logs` are appended, or over gRPC with `--otlp-transport grpc` (`OTEL_EXPORTER_OTLP_PROTOCOL_KIND`). `--otel-service-name` and `--otel-export-timeout-ms` complete the exporter settings; [`observability.md`](observability.md#enable-otlp-export) documents all of them. Standard OTLP header environment variables carry receiver authentication and routing without exposing credentials in process arguments. The short-lived runner flushes configured exporters before returning, and a failed flush fails the command.

Prompt text, model responses, model-authored script text and its output, provider input/output, bearer tokens, OTLP authorization headers, and raw errors are intentionally excluded from telemetry. Two opt-ins widen that on purpose: `--otel-telemetry-payloads true` adds the transcript events [`observability.md`](observability.md#model-and-tool-transcript) defines, and `--suggestions` records the model's bounded suggestion fields as `agent.improvement.suggested` in either mode. Lifecycle logs record stable command/session/model/script/guest events and share generated trace and span IDs with performance traces. They are operational audit telemetry, not broker authorization evidence or a replacement for durable broker audit. See [`observability.md`](observability.md) for configuration, event semantics, data minimization, and the single-container OpenObserve example.

Direct-operation bounds are configurable on `inspect`, `invoke`, `shell`, `prompt`, and `session replay` (for its `--provider` components) with:

- `--max-memory-bytes`
- `--max-input-bytes`
- `--max-output-bytes`
- `--fuel`
- `--timeout-ms`

`--max-memory-bytes` bounds each linear memory rather than the whole store, so the host also applies
fixed ceilings on table elements, tables, linear memories, and core instances per store. Those have
no flag: they close the `table.grow` path, where one cheap instruction under fuel accounting could
otherwise make the host allocate far past the memory bound. `--max-output-bytes` likewise bounds
what the host will parse, not peak allocation — the buffered-string contract lifts the whole guest
string into host memory before it can be measured, and the store's memory limits are what bound
that.

`--max-input-bytes` bounds a capability's input JSON and, for a provider command word, the argv
words plus the value piped into the word, counted before a store exists; a run past it never
reaches the guest and the script sees `<word>: failed: …` naming the bound, at exit `1`. Over
`--broker` the corresponding bound is the broker's own `maxInputBytes`, and its refusal reaches the
script at the same exit code carrying the broker's opaque `provider-error` reply, because the
broker names the bound only in its own `command.resolve.failed` record.

The host supplies no WASI, filesystem, network, environment, clock, random, credential, JSONL, or
durable-file imports. It accepts only capabilities declaring `read-only`. Consequently this path
exercises pure provider computation; the storage-importing components — the generated
`storage-probe-provider.wasm` and the fetched `memory-chat-provider.wasm` — are intentionally
rejected by `inspect`, just like HTTP-importing components.

## Authority limitation

A model tool call in direct `dekopon-run` mode is not an `AuthorizedInvocation`. Immediate mode performs no broker transition and must not be extended to provider credentials, host networking, local writes, or external writes. Explicit broker mode submits proposals without receiving effect authority; the authenticated, policy-controlled, separately deployed broker owns HTTP imports and execution as described in [`broker-http.md`](broker-http.md), [`design.md`](design.md), and [`security-model.md`](security-model.md).
