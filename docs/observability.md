# Runner observability

`dekopon-run` can currently export its execution traces and audit-safe lifecycle logs over OTLP/gRPC. This covers the one-shot runner process, model turns, immediate Wasm compilation/description/invocation, model-authored script execution in both `prompt` and `shell` modes, and explicit broker-client calls. It does **not** collect telemetry from `dekopon-brokerd`, Kubernetes nodes, or other Rust processes; log tailing or a Vector-based forwarding layer remains separate work.

This is operational observability for guest execution. It does not replace broker policy evidence, authorized invocation results, or the broker's durable hash-linked audit log.

## Enable OTLP export

Export remains disabled unless an endpoint is configured:

```console
dekopon-run \
  --otlp-endpoint http://quickwit:7281 \
  --otel-service-name dekopon-run \
  --otel-logs-index otel-logs-v0_9 \
  --otel-traces-index otel-traces-v0_9 \
  prompt ...
```

The global flags and environment equivalents are:

| CLI | Environment | Default |
|---|---|---|
| `--otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset; export disabled |
| `--otel-service-name` | `OTEL_SERVICE_NAME` | `dekopon-run` |
| `--otel-logs-index` | `DEKOPON_OTEL_LOGS_INDEX` | `otel-logs-v0_9` |
| `--otel-traces-index` | `DEKOPON_OTEL_TRACES_INDEX` | `otel-traces-v0_9` |
| `--otel-export-timeout-ms` | `DEKOPON_OTEL_EXPORT_TIMEOUT_MS` | `5000` |

The two index values are sent as Quickwit's `qw-otel-logs-index` and `qw-otel-traces-index` gRPC metadata. Quickwit 0.9 creates the two defaults automatically; operator-selected custom indexes must already have compatible OTEL mappings. Standard `OTEL_RESOURCE_ATTRIBUTES` values are attached to both signals. HTTPS endpoints use WebPKI roots; the kind fixture deliberately uses plaintext only on its isolated cluster network.

A short-lived runner uses batch exporters and explicitly shuts down both providers before returning. SDK-reported flush failures make the command fail instead of being silently ignored. `--trace <PATH>` can still produce a local Chrome/Perfetto trace alongside OTLP export.

## Trace and log model

One generated OpenTelemetry trace links the command to spans such as:

- `runner.command`, `runner.prompt`, `runner.shell`, and `prompt.session`;
- `prompt.model_turn` and `model.complete`;
- `prompt.script` and `shell.command`; and
- `provider.compile`, `provider.describe`, and `provider.invoke`.

One model turn drives at most a handful of scripts, and one script drives many capability calls, so `prompt.script` is the span for a whole unit of model-requested work rather than for a single capability invocation.

Inside it, `shell.command` is one span per command word the script actually ran, in execution order — a builtin, a capability call, a shell function, a word this shell refuses, or a word that resolved to nothing. A trace therefore reads as the ordered list of commands a script executed rather than as one opaque entry, and the reading survives constructs where one script word drives several executions: `xargs` mapping a command over ten items produces ten `shell.command` spans nested inside its own. The interpreter emits these as plain `tracing` spans and knows nothing about OTLP; `dekopon_shell` is already named in this file's trace and log filters, so they flow to every configured sink with no further wiring. Each span carries:

| Attribute | Value |
|---|---|
| `shell.command.name` | The command word, or `<withheld>`; see data minimization below |
| `shell.command.kind` | `builtin`, `capability`, `function`, `control`, `rejected`, or `not-found` |
| `shell.command.argument_count` | How many arguments the word received, never their values |
| `shell.command.exit_code` | The status the command reported |
| `outcome` | `succeeded`, `failed`, `denied`, `not-found`, `usage-error`, `timed-out`, `limit-exceeded`, or `rejected` |

`outcome` keeps a policy refusal (`denied`) distinct from a capability that ran and errored (`failed`) and from one that is unreachable (`not-found`), mirroring the interpreter's own exit-code mapping; flattening them would hide an authorization refusal in the noise of ordinary failures. `rejected` and `limit-exceeded` name the two ways a command ends the whole script — a construct this shell excludes, and an exhausted sandbox budget.

Structured log records use stable `audit.event` attributes for command, session, model-turn, script-execution, per-command, and direct guest-invocation lifecycle events. Each command emits a `shell.command.started` / `shell.command.completed` pair inside its span, the completed record adding `duration_ms` alongside the attributes above. Logs emitted inside the runner trace carry its generated `trace_id` and active `span_id`, allowing a Quickwit log result to pivot to the corresponding performance trace.

## Data minimization

Telemetry includes operation names, model/provider/capability identifiers, bounded counts, outcomes, durations, and source locations. It intentionally excludes:

- user and system prompts;
- model response text and reasoning replay data;
- model tool-call IDs and the script text a model authors, along with that script's output;
- provider input and output;
- command arguments, in every form and at every level;
- bearer tokens and provider credentials; and
- broker socket paths.

Command arguments deserve their own line because `shell.command` is the newest place they could have leaked. A `curl -d '{"apiKey":...}'` body and a `cap some.id '{"token":...}'` object are capability input wearing argv's clothes, so only the argument *count* is recorded. The command word itself is recorded only when it came from a fixed vocabulary the interpreter owns — a builtin name, a control word, a word the shell refuses by name, or a capability identifier. A shell function's name and a word that resolved to nothing are whatever the script's author typed, so both are reported as the literal `<withheld>` and the resolution kind is left to say what happened.

Model-selected invalid tool names are not copied into remote rejection events; a rejection records a stable category such as `unknown-tool` instead. Error telemetry records stable categories rather than raw errors, which may contain untrusted provider or transport text. Normal command stdout/stderr remains a separate output surface.

The immediate Wasm world has no logging import, so this records host-observed guest lifecycle and timing rather than arbitrary text emitted from inside a component.

## Quickwit development and CI

[`../deploy/quickwit-kind/`](../deploy/quickwit-kind/README.md) contains the pinned Quickwit 0.9/PostgreSQL kind stack. Quickwit's native OTLP service creates `otel-logs-v0_9` and `otel-traces-v0_9`; splits are stored on an ephemeral node-local `emptyDir`.

`tests/otel-kind/e2e.sh` runs an actual two-turn prompt/script session and searches both indexes. It verifies required spans and lifecycle events, shared trace IDs, topology, and payload redaction. CI runs this test in a fresh kind cluster.
