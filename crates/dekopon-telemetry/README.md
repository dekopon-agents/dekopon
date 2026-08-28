# dekopon-telemetry

Shared OTLP exporter construction and W3C trace context for Dekopon processes.

`dekopon-run`, `dekopon-brokerd`, and `dekopond` each export their own spans, so exporter
construction lives here rather than being duplicated in each binary. The crate depends on no other Dekopon crate: it
must stay linkable from the runner without pulling broker code into the runner's dependency tree,
which CI rejects.

## Subscriber installation

`Install` builds one process's whole subscriber: a `Console` layer — JSON or text, on stdout or
stderr, filtered by `RUST_LOG` or by a fixed directive — then any process-specific layer, then the
OTLP span layer, then the OTLP log bridge, in that order so an entered span has already activated
a context the log SDK can correlate against. It returns a `TelemetryGuard` whose `shutdown` flushes
and stops both providers, reporting every failure rather than the first. What a caller does with
that failure stays the caller's policy: the short-lived runner fails the command, and the daemons
log and carry on.

## Transports

`Transport::Grpc` and `Transport::Http` are both first-class. gRPC method paths are fixed by the
OTLP protobuf service definition, which suits a receiver reached through a path-routing reverse
proxy; HTTP appends `/v1/traces` and `/v1/logs` to the configured base endpoint. Both reach an
`https://` endpoint using WebPKI roots. The HTTP client is the workspace's own reqwest build with
redirects disabled, so an authorization header cannot be forwarded to a receiver-selected
destination, and one client serves both signals rather than one per signal.

## Export failures

The OpenTelemetry SDK reports its own export failures through its `internal-logs` feature, which
this crate enables. Those records use the `opentelemetry*` `tracing` targets, and `Install`
silences that prefix on every OTLP layer it builds whatever directive the calling binary supplies,
so an export failure reaches stdout or stderr and can never be re-exported through the exporter
that produced it.

## Authority

This crate configures transport and never resolves credentials. Ingest authentication is read by
the OpenTelemetry SDK from the standard `OTEL_EXPORTER_OTLP_HEADERS` environment variable, so a
token is never accepted as a command-line argument, never written to a configuration file this
crate parses, and never attached to a span attribute or log field. Endpoint URL userinfo is
rejected; ingest credentials must use the standard header variables and are never exposed by the
broker web UI.

## Trace context

`current_trace_context` reads the OpenTelemetry context of the active `tracing` span, and
`remote_context` rebuilds a remote parent from identifiers received over a wire protocol. The
crate speaks raw identifier bytes rather than a Dekopon wire type; `dekopon-broker-protocol` owns
`traceparent` parsing, formatting, and validation.
