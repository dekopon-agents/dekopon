# dekopon-http-host

Statically linked native implementation behind Dekopon's buffered HTTP provider primitive.

This crate is transport machinery for `dekopon-broker-host`, not a provider API and not an authorization engine. A `BufferedHttpClient` consumes one broker-produced `HttpConstraints` grant under independent `HttpHostCeilings`. Disabled contexts deny every call.

The client accepts arbitrary HTTP method tokens and ordered byte-valued headers, while enforcing exact methods and destination authorities, call and byte limits, representable deadlines, bounded public-address DNS checks and pinning, HTTPS by default, loopback-only opt-in plaintext with an explicit port, no redirects, no ambient proxies, no automatic decompression, and sensitive/hop-by-hop header filtering. Response bodies are streamed natively into a bounded buffer. Evidence contains only method, authority, status, and accounted byte counts—never paths, queries, headers, or bodies.

The crate deliberately knows nothing about WIT, Wasmtime stores, provider manifests, authenticated callers, policy evaluation, credentials, or audit persistence. Those boundaries remain in the broker layers.
