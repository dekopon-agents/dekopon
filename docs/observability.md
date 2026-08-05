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
- `prompt.script`; and
- `provider.compile`, `provider.describe`, and `provider.invoke`.

One model turn drives at most a handful of scripts, and one script drives many capability calls, so `prompt.script` is the span for a whole unit of model-requested work rather than for a single capability invocation. Per-builtin detail inside the interpreter is not recorded yet.

Structured log records use stable `audit.event` attributes for command, session, model-turn, script-execution, and direct guest-invocation lifecycle events. Logs emitted inside the runner trace carry its generated `trace_id` and active `span_id`, allowing a Quickwit log result to pivot to the corresponding performance trace.

## Data minimization

Telemetry includes operation names, model/provider/capability identifiers, bounded counts, outcomes, durations, and source locations. It intentionally excludes:

- user and system prompts;
- model response text and reasoning replay data;
- model tool-call IDs and the script text a model authors, along with that script's output;
- provider input and output;
- bearer tokens and provider credentials; and
- broker socket paths.

Model-selected invalid tool names are not copied into remote rejection events; a rejection records a stable category such as `unknown-tool` instead. Error telemetry records stable categories rather than raw errors, which may contain untrusted provider or transport text. Normal command stdout/stderr remains a separate output surface.

The immediate Wasm world has no logging import, so this records host-observed guest lifecycle and timing rather than arbitrary text emitted from inside a component.

## Quickwit development and CI

[`../deploy/quickwit-kind/`](../deploy/quickwit-kind/README.md) contains the pinned Quickwit 0.9/PostgreSQL kind stack. Quickwit's native OTLP service creates `otel-logs-v0_9` and `otel-traces-v0_9`; splits are stored on an ephemeral node-local `emptyDir`.

`tests/otel-kind/e2e.sh` runs an actual two-turn prompt/script session and searches both indexes. It verifies required spans and lifecycle events, shared trace IDs, topology, and payload redaction. CI runs this test in a fresh kind cluster.
