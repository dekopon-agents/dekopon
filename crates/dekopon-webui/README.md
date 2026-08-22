# dekopon-webui

`dekopon-webui` is the unauthenticated, read-only operational dashboard embedded in
`dekopon-brokerd`. Enable it explicitly with:

```console
dekopon-brokerd --config /path/to/broker.yaml --http-bind=0.0.0.0:8080
```

`/` permanently redirects to `/ui`. The dashboard shows gateway-reported catalog agents and
provider permissions, provider manifests and component interfaces, process-local model-token
accounting, host-observed Wasmtime counters and ceilings, and broker OTLP settings without header
or resource-attribute values. Provider pages render the complete validated manifest, input schemas,
artifact path, size, SHA-256, and Wasmtime-visible imports and exports.

The HTTP router has only `GET`/`HEAD` views and no login or mutation endpoint. “Read-only” does not
mean “non-sensitive”: agent names, provider schemas, local artifact paths, receiver endpoints, and
runtime capacity are deployment information. Binding to a non-loopback address is an explicit
operator decision, and the surrounding network must be treated as the access boundary.

Every response — including the 405 an unrouted method produces — carries `Cache-Control: no-store`,
`X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and a closed content-security
policy. One `tracing` event per request records method, path, status, and response bytes at `debug`,
so a production `info` filter ships nothing and no query string or body is ever recorded.

## Listener ceilings

The listener is an unauthenticated TCP surface inside the privileged broker process, whose worst
deployment failure is an OOM kill, so it is bounded like the broker's Unix socket rather than left
open-ended:

| Ceiling | Default | Behavior at the ceiling |
|---|---|---|
| `max_connections` | 16 | The connection is **closed without a response**; it is never queued. |
| `connection_timeout` | 30s | The connection is dropped, whatever stage it reached. |

The deadline is absolute from accept, not per read or per write: a slow-reading client pins a whole
rendered response, so only a budget spanning the entire connection — header read, render, body
write, and HTTP/1 keep-alive reuse — bounds it. Sixteen is generous for pages that inline their own
stylesheet and load no subresources, so a browser needs one connection per view. `serve` applies
these defaults; `serve_with_limits` takes them explicitly and refuses a zero on either.

Agent and token data remain owned by the unprivileged gateway. `dekopond` sends bounded
informational reports over the authenticated Unix broker protocol; the broker retains them only in
memory and never consults them for Cedar policy, constraints, credentials, routing, evidence, or
durable audit. Counters and the latest inventory reset when `dekopon-brokerd` restarts.

Storage-backed invocations add only content-free operation/sync/quota counts and coarse
powers-of-two byte buckets. Exact storage provider input/output totals, provider/capability,
agent/subject/scope, logical names, offsets, queries, root/key paths, and opaque tokens never enter
UI state.
