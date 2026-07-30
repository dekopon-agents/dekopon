# dekopon-capability

Capability descriptors and invocation typestates for Dekopon.

The API distinguishes model-proposed invocations from broker-authorized invocations. An authenticated envelope will carry proposals into the broker; trusted broker code owns an `AuthorizationGate` and creates and consumes non-cloneable `AuthorizedInvocation` values only after authentication and policy checks. Their serialized representation is inert audit/evidence data, not executable authority for a caller to present back to the broker. Public `InvocationResult` values carry a deserializable `DecisionReference` so clients can correlate decision ID, broker principal, and policy revision without receiving authorization.

`ExecutionConstraints` can carry an optional deny-by-default buffered HTTP grant with exact hosts and methods plus positive request-count and byte limits. Its absence permits no HTTP host calls. The broker host applies those values beneath independent process ceilings.

Private authorization fields, single-use ownership, and the intentional absence of deserialization provide defense in depth. Constructing an `AuthorizationGate` is an explicit API transition for trusted broker adapters, not proof that a caller was authenticated or policy was evaluated. They do not replace broker process isolation, authenticated messages, replay protection, policy enforcement, or binding authorization to execution.
