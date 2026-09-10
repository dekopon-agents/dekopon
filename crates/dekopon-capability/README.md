# dekopon-capability

Capability descriptors and invocation typestates for Dekopon.

The API distinguishes model-proposed invocations from broker-authorized ones ([constitution](../../docs/design.md#constitution), invariant 1). An authenticated envelope carries proposals into the broker — `dekopon-broker-protocol` over a private Unix socket, with no identity or authority fields in the payload; trusted broker code owns an `AuthorizationGate` and creates and consumes non-cloneable `AuthorizedInvocation` values only after authentication and policy checks. Their serialized representation is inert audit and evidence data, not executable authority for a caller to present back to the broker.

An `AuthorizedInvocation` carries an `AuthorizationReceipt` — decision ID, authorizing broker principal, policy revision — and `Evidence` entries carry a kind, a digest of canonical bytes, and a media type. Public `InvocationResult` values carry a deserializable `DecisionReference` instead, so a client correlates a decision without receiving authority. *Committed direction:* removed; the trace is the record ([design.md](../../docs/design.md#core-concepts)).

`ExecutionConstraints` can carry an optional deny-by-default buffered HTTP grant with exact hosts and methods plus positive request-count and byte limits. Its absence permits no HTTP host calls. The broker host applies those values beneath independent process ceilings.

`HttpConstraints::validate` owns the entry grammar those fields promise — exact authorities, exact HTTP method tokens, and the per-list entry cap — and the gate applies it. `dekopon-broker` and `dekopon-http-host` call the same check, so no construction path accepts a grant the enforcing host would refuse.

Private authorization fields, single-use ownership, and the absence of deserialization are defense in depth. Constructing an `AuthorizationGate` is an explicit API transition for trusted broker adapters, not proof that a caller was authenticated or policy was evaluated. It replaces none of broker process isolation, authenticated messages, replay protection, policy enforcement, or binding authorization to execution.
