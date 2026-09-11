# dekopon-telemetry

Shared OTLP exporter construction and W3C trace context for Dekopon processes.

`dekopon-brokerd` and `dekopond` each export their own spans, so exporter construction lives here
rather than in each binary. The crate depends on no other Dekopon crate: it must stay linkable from
the gateway without pulling broker code into the gateway's dependency tree, which CI rejects.

## Subscriber installation

`Install` builds one process's whole subscriber: a `Console` layer — JSON or text, on stdout or
stderr, filtered by `RUST_LOG` or by a fixed directive — then any process-specific layer, then the
OTLP span layer, then the OTLP log bridge, in that order so an entered span has already activated
a context the log SDK can correlate against. It returns a `TelemetryGuard` whose `shutdown` flushes
and stops both providers, reporting every failure rather than the first. What a caller does with
that failure stays the caller's policy: the daemons log and carry on.

## Transports

`Transport::Grpc` and `Transport::Http` are both first-class. gRPC method paths are fixed by the
OTLP protobuf service definition, which suits a receiver reached through a path-routing reverse
proxy; HTTP appends `/v1/traces` and `/v1/logs` to the configured base endpoint. Both reach an
`https://` endpoint using WebPKI roots. The HTTP client is the workspace's own reqwest build with
redirects disabled, so an authorization header cannot be forwarded to a receiver-selected
destination, and with ambient proxies disabled, so an exported `HTTPS_PROXY` cannot put the
`OTEL_EXPORTER_OTLP_HEADERS` ingest credential — or a span — on a host nobody named to Dekopon. A
collector reachable only through a proxy must be addressed directly; one client serves both signals
rather than one per signal.

## Export failures

The OpenTelemetry SDK reports its own export failures through its `internal-logs` feature, which
this crate enables. Those records use the `opentelemetry*` `tracing` targets, and `Install`
silences that prefix on every OTLP layer it builds whatever directive the calling binary supplies,
so an export failure reaches stdout or stderr and can never be re-exported through the exporter
that produced it.

## Export queues

Both batch processors are configured rather than left on the SDK's defaults: 1024 spans and 256 log
records queued, drained in batches of 256 and 64. The SDK's default is 2048 records per queue and it
has no byte ceiling anywhere — `BatchConfig` counts records and `SpanLimits` counts attributes, and
nothing truncates an attribute value — so a queue's size is `records × the largest attribute the
process emits`. Under [goal 2](../../docs/design.md#constitution) that attribute is a prompt, a
model answer, or a whole script's 256 KiB of output, which put the log queue's worst case at half a
gigabyte. The log queue is the tighter of the two because that is where the bytes are; the span
queue keeps four drains of headroom because a span is never dropped. Records are still dropped if a
receiver stalls long enough to fill a queue, and the SDK reports the total at shutdown.

## Authority

This crate configures transport and never resolves credentials. Ingest authentication is read by
the OpenTelemetry SDK from the standard `OTEL_EXPORTER_OTLP_HEADERS` environment variable, so a
token is never accepted as a command-line argument, never written to a configuration file this
crate parses, and never attached to a span attribute or log field. Endpoint URL userinfo is
rejected; ingest credentials must use the standard header variables.

The telemetry store sits inside the operator's trust boundary
([`docs/design.md#constitution`](../../docs/design.md#constitution)). *Committed direction:* the
gate is removed; payloads always on, command words and arguments are recorded, and no span is
dropped ([exclusions](../../docs/observability.md#exclusions)).

## Trace context

`current_trace_context` reads the OpenTelemetry context of the active `tracing` span, and
`remote_context` rebuilds a remote parent from identifiers received over a wire protocol. The
crate speaks raw identifier bytes rather than a Dekopon wire type; `dekopon-broker-protocol` owns
`traceparent` parsing, formatting, and validation.

`current_trace_context` answers `None` whenever no OpenTelemetry layer is installed, which is every
process that configured no OTLP trace exporter: the identifiers are absent rather than invalid, and
no depth of span nesting produces them. A caller that must put a trace on a wire mints its own
instead — see `dekopon_agent::session_trace_parent`.

[`docs/observability.md`](../../docs/observability.md#trace-context-across-the-socket) is the
authoritative account of the trace identifier every record carries.
