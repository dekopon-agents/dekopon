# dekopon-telemetry

Shared OTLP exporter construction and W3C trace context for Dekopon processes.

`dekopon-run`, `dekopon-brokerd`, and `dekopond` each export their own spans, so exporter
construction lives here rather than being duplicated in each binary. The crate depends on no other Dekopon crate: it
must stay linkable from the runner without pulling broker code into the runner's dependency tree,
which CI rejects.

## Transports

`Transport::Grpc` and `Transport::Http` are both first-class. gRPC method paths are fixed by the
OTLP protobuf service definition, which suits a receiver reached through a path-routing reverse
proxy; HTTP appends `/v1/traces` and `/v1/logs` to the configured base endpoint. The HTTP client is
the workspace's own reqwest build with redirects disabled, so an authorization header cannot be
forwarded to a receiver-selected destination.

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
