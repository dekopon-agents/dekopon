# dekopon-http-host

Statically linked native implementation behind Dekopon's buffered HTTP provider primitive.

This crate is transport machinery for `dekopon-broker-host`, not a provider API and not an authorization engine. A `BufferedHttpClient` consumes one broker-produced `HttpConstraints` grant under independent `HttpHostCeilings`. Disabled contexts deny every call.

The client accepts arbitrary HTTP method tokens and ordered byte-valued headers, while enforcing exact methods and destination authorities, call and byte limits, representable deadlines, bounded public-address DNS checks and pinning, HTTPS by default, loopback-only opt-in plaintext with an explicit port, no redirects, no ambient proxies, no automatic decompression, and sensitive/hop-by-hop header filtering. A separately authorized public DRN adds exact native sink/binding identity, canonical path/query and injection constraints; the host renders strict Basic/Bearer and discards direct raw/rendered credential reflection. Response bodies are streamed natively into a bounded buffer.

The address bound is a ceiling on the pin set rather than an admission test on the resolver answer: duplicates collapse first and the remainder is truncated, so a dual-stack round-robin destination stays reachable, and every retained address is still validated and pinned. Within one execution context, resolution and the built client are reused while the pin set is unchanged—the cache key is the whole `(host, addresses)` pair, so a multi-call capability shares one connection without a client ever being reused for addresses it was not built to reach. Destination authorities use the URL grammar throughout, so an IPv6 literal is written bracketed (`[::1]:8080`).

Evidence contains only method, authority, status, and accounted byte counts—never paths, queries, headers, or bodies. An entry exists from the point a request is dispatchable, so a call the credential binding then refuses is still recorded, status-less. A call rejected earlier—unauthorized method, denied destination, invalid header, failed resolution—consumes a unit of the request budget but has no sanitized authority to name; its failure class reaches telemetry through the `http.request` span and the `accounting.http.request` record instead, which are emitted for every attempt.

The crate deliberately knows nothing about WIT, Wasmtime stores, provider manifests, authenticated callers, policy evaluation, credentials, or audit persistence. Those boundaries remain in the broker layers.
