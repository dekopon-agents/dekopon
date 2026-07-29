# dekopon-capability

Capability descriptors and invocation typestates for Dekopon.

The API distinguishes model-proposed invocations from broker-authorized invocations. A future authenticated envelope will carry proposals into the broker; the broker-owned execution boundary creates and consumes `AuthorizedInvocation` values. Their serialized representation is inert audit/evidence data, not executable authority for a caller to present back to the broker.

`ExecutionConstraints` can carry an optional deny-by-default buffered HTTP grant with exact hosts and methods plus positive request-count and byte limits. Its absence permits no HTTP host calls. The broker host applies those values beneath independent process ceilings.

Rust visibility and the intentional absence of deserialization provide defense in depth. They do not replace broker process isolation, authenticated messages, replay protection, policy enforcement, or binding authorization to execution.
