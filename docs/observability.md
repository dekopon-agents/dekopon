# Observability

`dekopon-run` and `dekopon-brokerd` each export their own execution traces over OTLP, using either
gRPC or HTTP with protobuf payloads. Runner coverage is the one-shot runner process, model turns,
immediate Wasm compilation/description/invocation, model-authored script execution in both `prompt`
and `shell` modes, and explicit broker-client calls. Broker coverage is one span per decoded
invocation from a mapped peer. Neither collects telemetry from Kubernetes nodes or other Rust
processes; host-level collection remains separate work.

The two processes export **independently**. The broker only ever observes broker-mediated
invocations, so it cannot stand in for the runner: a broker-only deployment loses every model turn,
every direct-mode capability call, and every script span. They are separate emitters that meet in
the backend, correlated by trace context rather than by one relaying for the other.

This is operational observability. It does not replace broker policy evidence, authorized
invocation results, or the broker's durable hash-linked audit log.

## Enable OTLP export

Export remains disabled unless an endpoint is configured:

```console
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:5080/api/default
export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20<INGESTION_TOKEN>,stream-name=dekopon'
export OTEL_SERVICE_NAME=dekopon-run

dekopon-run prompt ...
```

The global flags and environment equivalents are:

| CLI | Environment | Default |
|---|---|---|
| `--otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset; export disabled |
| `--otlp-transport` | `OTEL_EXPORTER_OTLP_PROTOCOL_KIND` | `http` |
| `--otel-service-name` | `OTEL_SERVICE_NAME` | `dekopon-run` |
| `--otel-export-timeout-ms` | `DEKOPON_OTEL_EXPORT_TIMEOUT_MS` | `5000` |

Both transports are first-class. `http` treats the endpoint as a generic OTLP/HTTP base and appends
`/v1/traces` and `/v1/logs`. `grpc` treats it as an authority and takes its method paths from the
OTLP protobuf service definition, which is what a receiver behind a path-routing reverse proxy
needs — those paths are fixed by the protocol and cannot be reassigned, so the proxy rule matches
`/opentelemetry.proto.collector.*` rather than a path of the operator's choosing.

Both read the standard `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, and
`OTEL_EXPORTER_OTLP_LOGS_HEADERS` variables directly through the exporter. Header values use the
OpenTelemetry URL-encoded form; for example, `%20` represents the space in `Basic <token>`. There is
intentionally no header CLI flag and no header configuration field, because credentials must not be
exposed in process arguments, retained in a parsed CLI value, or written into a configuration file.

Standard `OTEL_RESOURCE_ATTRIBUTES` values are attached to both signals. HTTPS endpoints use WebPKI roots, and redirects are disabled so a receiver cannot forward an authorization header to another destination. Plain HTTP is suitable only for a loopback development receiver or an otherwise trusted isolated network because headers and telemetry are unencrypted.

## Broker export

`dekopon-brokerd` exports through an optional `telemetry` section in its owner-controlled
configuration. The section is absent by default, and when present every field is required, matching
every other section in that file:

```yaml
telemetry:
  endpoint: http://rpi.localdomain
  transport: grpc            # grpc | http
  serviceName: dekopon-brokerd
  exportTimeoutMs: 5000
```

There is deliberately no credential field. The broker reads `OTEL_EXPORTER_OTLP_HEADERS` like the
runner does, so a token never enters the configuration file the broker parses, its command line, or
any span attribute — the same rule that keeps provider credentials out of prompts and audit fields.

Telemetry never blocks startup. An exporter that cannot be built disables export and logs why;
authorization and durable audit are the service's contract, and a missing dashboard must not cost a
working authority boundary. Flush failures at shutdown are logged and do not change the exit code,
because the audit chain rather than telemetry is the record of what happened.

The broker's log output is structured JSON on stdout, filtered by `RUST_LOG` and defaulting to
`info`. Shipping those logs to storage is deliberately left to whatever reads stdout, so the broker
holds one credential rather than two.

## Trace context across the socket

`InvocationRequest` carries an optional W3C `traceParent`. The runner fills it from the span that
actually requested the capability, and the broker opens `broker.invocation` beneath it as a remote
parent, so one trace spans both processes instead of two unrelated traces appearing per run.

This is separate from `TraceId`, which continues to identify a Dekopon session in the audit chain
and replay accounting. Two identifiers, two jobs: `TraceId` is durable audit correlation and
`traceParent` is telemetry correlation.

`traceParent` is untrusted like every other request field. It reaches span parenting and nothing
else — never policy, replay rejection, routing, or audit. A malformed value is a decode failure
rather than a silent `None`, since attaching broker spans to a trace that does not exist is worse
than sending none; an absent value simply means the client exports no telemetry.

The broker span carries the invocation, capability, and trace identifiers and nothing more. Provider
input and output, URL paths and queries, headers, and bodies stay out of it for exactly the reason
they stay out of audit records: telemetry is a second egress path with none of the audit chain's
guarantees, and it must not carry what audit deliberately redacts.

## Broker execution spans

`broker.invocation` is not a flat bar. Beneath it the broker's own crates emit:

| Span | Crate | Fields |
|---|---|---|
| `broker.authorize` | `dekopon-broker` | invocation, capability, `outcome` (`allowed`, `policy-denied`, `replayed-invocation`) |
| `broker.execute` | `dekopon-broker` | provider |
| `provider.compile` | `dekopon-broker-host` | none; emitted once per provider at startup |
| `provider.invoke` | `dekopon-broker-host` | capability, provider |
| `http.request` | `dekopon-http-host` | `http.request.method`, `server.address`, `http.response.status_code`, request/response body sizes, `outcome` |

`http.request` fields mirror `HttpCallEvidence` exactly, and that is deliberate rather than
incidental: the span reports the same call the audit chain records, so it carries the same sanitized
set and no more. URL paths and queries, request and response headers, and both bodies are absent
here for the same reason they are absent from evidence. A test in `dekopon-http-host` drives a real
loopback request whose path, query, header, and body are each a distinct sentinel and asserts that
none of them reach a span field.

`provider.compile` covers startup component compilation rather than per-invocation work, so it
answers "why was the broker slow to become ready" rather than "why was that call slow".

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

Structured log records use stable `audit.event` attributes for command, session, model-turn, script-execution, per-command, and direct guest-invocation lifecycle events. Each command emits a `shell.command.started` / `shell.command.completed` pair inside its span, the completed record adding `duration_ms` alongside the attributes above. Logs emitted inside the runner trace carry its generated `trace_id` and active `span_id`, allowing an OTLP log result to pivot to the corresponding performance trace.

## Data minimization

Telemetry includes operation names, model/provider/capability identifiers, bounded counts, outcomes, durations, and source locations. It intentionally excludes:

- user and system prompts;
- model response text and reasoning replay data;
- model tool-call IDs and the script text a model authors, along with that script's output;
- provider input and output;
- command arguments, in every form and at every level;
- bearer tokens, OTLP authorization headers, and provider credentials; and
- broker socket paths.

Command arguments deserve their own line because `shell.command` is the newest place they could have leaked. A `curl -d '{"apiKey":...}'` body and a `cap some.id '{"token":...}'` object are capability input wearing argv's clothes, so only the argument *count* is recorded. The command word itself is recorded only when it came from a fixed vocabulary the interpreter owns — a builtin name, a control word, a word the shell refuses by name, or a capability identifier. A shell function's name and a word that resolved to nothing are whatever the script's author typed, so both are reported as the literal `<withheld>` and the resolution kind is left to say what happened.

Model-selected invalid tool names are not copied into remote rejection events; a rejection records a stable category such as `unknown-tool` instead. Error telemetry records stable categories rather than raw errors, which may contain untrusted provider or transport text. Normal command stdout/stderr remains a separate output surface.

The immediate Wasm world has no logging import, so this records host-observed guest lifecycle and timing rather than arbitrary text emitted from inside a component.

## OpenObserve development and CI

[`../examples/otel-traces/`](../examples/otel-traces/README.md) starts one pinned OpenObserve container with one Docker volume, documents authenticated OTLP/HTTP export, and explains how to inspect traces in the UI.

`examples/otel-traces/smoke-test.sh` is the repository-level black-box check. It starts an isolated OpenObserve instance, executes a real direct provider invocation, searches the trace stream, and asserts that the root runner span and provider spans arrived without the sentinel provider input. CI runs the same script and removes the container and volume afterward.
