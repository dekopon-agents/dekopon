# dekopon-http-host

Statically linked native implementation behind Dekopon's buffered `send` and asset-backed `stream` HTTP provider primitives (`dekopon:http@1.1.0`).

This crate is transport machinery for `dekopon-broker-host`, not a provider API and not an authorization engine. A `BufferedHttpClient` consumes one broker-produced `HttpConstraints` grant under independent `HttpHostCeilings`. Disabled contexts deny every call.

The client accepts arbitrary HTTP method tokens and ordered byte-valued headers, while enforcing exact methods and destination authorities, call and byte limits, representable deadlines, bounded public-address DNS checks and pinning, HTTPS by default, loopback-only opt-in plaintext with an explicit port, no redirects, no ambient proxies, no automatic decompression, and sensitive/hop-by-hop header filtering. A separately authorized public DRN adds exact native sink/binding identity, canonical path/query and injection constraints; the host renders strict Basic/Bearer and its credential echo check refuses any response that carries the raw or encoded credential. Buffered `send` responses use a bounded buffer. `stream` sends literal/asset parts with exact Content-Length and spools responses to bounded disk-backed handles, scanning credentials before guest exposure; the request grant counts literal bytes, while asset admission has separate decoded limits. The broker continues linking buffered HTTP `@1.0.0` for older components.

The address bound is a ceiling on the pin set rather than an admission test on the resolver answer: duplicates collapse first and the remainder is truncated, so a dual-stack round-robin destination stays reachable, and every retained address is validated and pinned. Within one execution context, resolution and the built client are reused while the pin set is unchanged—the cache key is the whole `(host, addresses)` pair, so a multi-call capability shares one connection without a client ever being reused for addresses it was not built to reach. Destination authorities use the URL grammar throughout, so an IPv6 literal is written bracketed (`[::1]:8080`).

A `BoundCredential` may carry one fixed companion header beside `authorization`, which is what the broker's `chatgptSubscription` credential kind uses for `chatgpt-account-id`. It is one credential, not a generic header sink: the companion is rendered and inserted under the same destination-binding decision as the bearer token, a guest that sets its name is refused rather than overwritten exactly as for `authorization`, its bytes are outside the guest's accounted request size, and `HttpCallEvidence` contains no field for it. *Committed direction:* the broker's `credential`/`agents.<id>.credentials` bindings will be replaced by public DRNs, preserving this destination-bound injection ([migration requirements](../../docs/design.md#legacy-credential-bindings)).

Evidence contains only method, authority, status, and accounted byte counts—never paths, queries, headers, or bodies. An entry exists from the point a request is dispatchable, so a call the credential binding then refuses is recorded, status-less. A call rejected earlier—unauthorized method, denied destination, invalid header, failed resolution—consumes a unit of the request budget but has no sanitized authority to name; its failure class reaches telemetry through the `http.request` span and the `accounting.http.request` record instead, which are emitted for every attempt.

The crate knows nothing about WIT, Wasmtime stores, provider manifests, authenticated callers, policy evaluation, credentials, or audit persistence. Those boundaries live in the broker layers.

## Request and credential boundary

The buffered interface carries an absolute URI, method token, ordered duplicate-preserving
headers and byte body; responses carry status, ordered headers and bounded bytes. It exposes
no guest sockets, DNS, arbitrary filesystem, environment or raw credential imports.
The additive streaming interface consumes bounded asset handles, not guest paths or network sockets.
Userinfo and URI fragments are refused. Header syntax/count/bytes, request body and complete
encoded request size, remaining calls, exact authority/effective port and method are enforced.
Authority-defining, hop-by-hop, proxy and broker-managed credential headers are not guest controlled.
Guest `traceparent` and `tracestate` headers are always refused with `InvalidHeader`. When the
grant opts in with `propagateTrace`, buffered and streaming requests inject the current
`http.request` span's W3C `traceparent` outside guest byte accounting. No header is sent without
an OTel context, and `tracestate` is never sent. Operators opt in only first-party destinations
inside their trust boundary, using a separate constraint set from third-party destinations.

Destination-bound credentials are injected only after guest-header validation; a guest
`authorization` header is rejected, not overwritten. Binding refusal never falls back to an
unauthenticated request. The injected header is outside guest byte grants and public evidence
sizes; evidence records only `credentialInjected`. The broker owns selection and `Redacted`
values, never the component. Public DRNs additionally require the separately authorized native
sink/use binding described in [secrets](../../docs/secrets.md). *Committed direction:* removed in
favor of public DRNs ([decisions](../../docs/design.md#accepted-implementation-decisions)).
