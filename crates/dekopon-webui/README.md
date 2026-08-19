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

Agent and token data remain owned by the unprivileged gateway. `dekopond` sends bounded
informational reports over the authenticated Unix broker protocol; the broker retains them only in
memory and never consults them for Cedar policy, constraints, credentials, routing, evidence, or
durable audit. Counters and the latest inventory reset when `dekopon-brokerd` restarts.
