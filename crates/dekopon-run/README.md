# dekopon-run

One-shot direct execution for read-only Dekopon WebAssembly providers plus an explicit unprivileged client for a separate broker.

- `dekopon-run inspect` validates direct provider manifests.
- `dekopon-run invoke` directly invokes and times an import-free read-only capability.
- `dekopon-run shell` runs one sandboxed [`dekopon-shell`](../dekopon-shell/README.md) script whose command words dispatch to provider capabilities instead of operating-system processes. Provider loading and the unchanged synchronous interpreter run on Tokio's blocking pool as one opaque, joined, non-interruptible [`dekopon-process`](../dekopon-process/README.md) node.
- `dekopon-run prompt` gives an OpenAI-compatible endpoint or a ChatGPT/Codex subscription one scripting tool instead of one tool per capability, optionally reaching a running broker with `--broker` for capabilities direct mode cannot serve. `--skill <DIRECTORY>` (repeatable) mounts an Agent Skills directory holding a `SKILL.md`: the model sees each skill's name and description and reads the rest on demand through `read_skill`; a directory that does not load, or two skills with one name, fails the command with exit `1` before any model call. `--suggestions` offers `suggest_improvement`, off by default because its record carries model-authored text into telemetry in either payload mode; what the model recorded is printed to standard error after the answer — one `suggestion i/n [category, confidence confidence] target: summary` line with indented `evidence:` and `proposal:` lines each — so standard output stays the answer.
- `dekopon-run broker capabilities` inspects the capabilities broker policy allows this authenticated Unix peer.
- `dekopon-run broker invoke` submits one identity-free, caller-ID-bearing proposal to `dekopon-brokerd`.
- `dekopon-run chat` holds a conversation with a running [`dekopond`](../dekopond/README.md) over its local development socket. It loads no component and runs no model or tool loop of its own: unlike `prompt`, which runs that loop in process, `chat` sends one JSON line per message and prints the daemon's reply.
- `dekopon-run session list` groups the `accounting.model.turn` records an OpenObserve stream holds by trace, newest first, and prints `TRACE`, `STARTED`, `TURNS`, `TOKENS`, `OUTCOME`, and `SERVICE` for at most `--limit` (default `50`) sessions, or the same rows with `--json`. Accounting fires in either payload mode, so it lists sessions recorded metadata-only.
- `dekopon-run session show` reconstructs one session's transcript from its exported `agent.model.prompt`, `agent.model.answer`, and `accounting.model.turn` records (`--trace-id`) or from a file (`--from-file`; exactly one of the two), and prints it as text or, with `--json`, as the document `replay --from-file` reads back.
- `dekopon-run session replay` puts a recorded session to a model again and answers every script the model writes from the recording, so no capability runs and no effect happens unless `--provider` components are supplied for the point where the model diverges. It takes `prompt`'s model flags, `--system`/`--system-file`, `--skill`, `--suggestions`, `--max-steps` (default `8`), and, for `--provider` components, `--compile-cache` and the Wasm and `--shell-*` bounds.
- `dekopon auth chatgpt` manages Dekopon's isolated subscription login.
- `--provider` takes a component file or a directory of them; a directory loads every `*.wasm` directly inside it, in filename order. Load order decides route-table construction, so the sort is what keeps repeated runs over one directory identical. Unlike `dekopon-brokerd`, the runner applies no ownership check: it loads components the invoking user already owns, under their own authority.
- `--trace <PATH>` exports Chrome/Perfetto-compatible spans without inputs or outputs, unless `--otel-telemetry-payloads true` opts that file in like any other sink.
- `--otlp-endpoint <URL>` exports correlated OTLP traces and audit-safe lifecycle logs, over HTTP/protobuf by default or gRPC with `--otlp-transport grpc` (`OTEL_EXPORTER_OTLP_PROTOCOL_KIND`); standard OTLP header environment variables carry receiver authentication and routing.

Every **direct** provider call uses a fresh bounded Wasmtime store with no WASI or custom imports. The echo implementation is released independently from [`dekopon-provider-echo`](https://github.com/dekopon-agents/dekopon-provider-echo); fetch core's exact v0.1.0 test fixture first:

```console
ci/fetch-external-provider-components.sh examples/providers echo
cargo run -p dekopon-run -- inspect \
  --provider examples/providers/echo-provider.wasm
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}'
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.reverse --input '{"message":"stressed"}'

OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:5080/api/default \
OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20<INGESTION_TOKEN>,organization=default,stream-name=dekopon' \
  cargo run -p dekopon-run -- invoke \
    --provider examples/providers/echo-provider.wasm \
    echo.echo --input '{"message":"observed"}'
```

The generic endpoint receives `/v1/traces` and `/v1/logs` suffixes. Prompts, model responses, model-authored script text and its output, provider input/output, credentials, and raw errors are excluded; `--otel-telemetry-payloads true` adds the transcript events `session show` and `session replay` read back, and `--suggestions` records the model's bounded suggestion fields in either mode. See [`../../docs/observability.md`](../../docs/observability.md) for the signal model and [`../../examples/otel-traces/`](../../examples/otel-traces/README.md) for the single-container OpenObserve example.

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

`dekopon-run session` reads sessions back from the OpenObserve stream the runner and `dekopond` export to, and replays one against a model with an operator's change applied — the loop [`../../docs/improvement.md`](../../docs/improvement.md) describes. `list`, and `show` or `replay` given `--trace-id`, share the receiver flags. `--openobserve-url <URL>` (or `DEKOPON_OPENOBSERVE_URL`) is the organization base the OTLP exporter posts to, such as `http://127.0.0.1:5080/api/default`, and must carry no query, fragment, or userinfo; missing, the command fails with `no OpenObserve URL; pass --openobserve-url or set DEKOPON_OPENOBSERVE_URL`. `--openobserve-stream <STREAM>` (or `DEKOPON_OPENOBSERVE_STREAM`; default `dekopon`) is the log stream, letters, digits, and underscores only. `--openobserve-auth-env <NAME>` (default `DEKOPON_OPENOBSERVE_AUTHORIZATION`) names the environment variable holding the complete `Authorization` header value, under the rule every other Dekopon credential follows: a name may appear in an argument, a value never does, and an unset variable is the failure. `--openobserve-timeout-ms <MILLISECONDS>` (default `10000`) is the deadline for each search request, and `--since <DURATION>` (default `7d`; a count followed by `s`, `m`, `h`, or `d`, zero refused) the window searched. A `--from-file` source reads a transcript `session show --json` printed and needs none of them.

The client posts `{"query": {"sql", "start_time", "end_time", "from", "size"}}` to `<base>/_search?type=logs` and nothing else — no ingestion, no stream management — and treats what comes back as untrusted: it follows no redirects and uses no ambient proxy, so the credential header cannot be forwarded to a host nobody named; it reads pages of 500 records and follows at most 20, then warns on standard error to narrow `--since`; each response is read to at most 32 MiB; a failure status is reported with a control-stripped excerpt of the body no longer than 1024 characters; and a trace identifier is checked against `[A-Za-z0-9._-]{1,128}` before it is interpolated into the search SQL. `audit.event` is read under the `audit_event` name OpenObserve folds it to as well as its own.

`session show --json` prints, and `replay --from-file` reads, one JSON document: `traceId`; `system`, the leading system messages in order (standing instructions, then any skills listing); `history`, the `[{user, answer}]` exchanges a persistent route replayed ahead of the prompt; `prompt`; `turns`, `[{turn, content, toolCalls: [{id, name, arguments, result}], usage, durationMs}]`; and `answer`. Absent optional fields are omitted, and a `--from-file` transcript or `--system-file` is read whole as UTF-8, at most 64 MiB. `show` and `replay` need a transcript, so the session must have run with payload telemetry on; one that did not fails with `has N accounted model turn(s) but no transcript`, naming the trace.

Replay puts the recorded system messages (unless `--system` or `--system-file` replaces them all), the earlier exchanges, and the prompt to the model again, and answers a script from the first unconsumed recording of exactly that text, wherever it sits, so a model that reorders two independent scripts stays on the recorded trajectory. The first script the recording cannot answer is the **divergence**, and replay does not invent tool output for it. Without `--provider` the model is answered `[replay stopped: the recorded session never ran this script and no live providers were supplied to run it]`, the session ends there, the report's `divergence.handling` is `stopped`, and the exit code is `0`: the turns before it are a faithful comparison. With `--provider` components the script runs live in direct mode — import-free, read-only, no network, under the same Wasm and `--shell-*` bounds as `prompt` — `handling` is `live`, only that first divergence is reported, and from there the report describes a new session rather than a comparison. `--skill` mounts skills as `prompt` does and drops any skills listing the recording carried; `--suggestions` offers `suggest_improvement` to the replayed model and prints its notes to standard error. The report is `{traceId, recorded, replayed, divergence, suggestions, error}` with `--json`, or a text comparison of both sessions' scripts index by index (`same`, `differs`, `recorded only`, `replayed only`) ending with both answers; it is printed either way, and the exit code is `1` only when the replayed session failed for a reason other than a divergence stop.

```console
export DEKOPON_OPENOBSERVE_URL=http://127.0.0.1:5080/api/default
# DEKOPON_OPENOBSERVE_AUTHORIZATION holds the Authorization header value.
dekopon-run session list --since 24h
dekopon-run session show --trace-id 4bf92f3577b34da6a3ce929d0e0e4736 --json > session.json
dekopon-run session replay --from-file session.json --model "$MODEL" \
  --system-file instructions.md --skill examples/local/skills/pull-request-review
```

`--skill` and `session` are why `dekopon-run` depends on `dekopon-config` (the catalog's skill loader), `ureq` (the search client), and `time` (RFC 3339 rendering for `session list`); it still depends on no privileged broker crate, and CI's `cargo tree` gate rejects `dekopon-broker`, `dekopon-broker-host`, `dekopon-brokerd`, `dekopon-http-host`, `dekopon-storage-host`, and `dekopon-policy` in its normal dependency graph.

ChatGPT subscription mode uses OpenAI's device authorization and Codex Responses transport directly; it does not import credentials from pi, OpenClaw, or the Codex CLI. `dekopon-run` remains an unprivileged client, not an authorization broker. Direct mode rejects local and external provider writes and never resolves provider credentials. See [`../../docs/run.md`](../../docs/run.md) and [`../../docs/broker-http.md`](../../docs/broker-http.md) for the full contracts.
