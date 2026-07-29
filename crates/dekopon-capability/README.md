# dekopon-capability

Capability descriptors and invocation typestates for Dekopon.

The API distinguishes model-proposed invocations from broker-authorized invocations. A future authenticated envelope will carry proposals into the broker; the broker-owned execution boundary creates and consumes `AuthorizedInvocation` values. Their serialized representation is inert audit/evidence data, not executable authority for a caller to present back to the broker.

Rust visibility and the intentional absence of deserialization provide defense in depth. They do not replace broker process isolation, authenticated messages, replay protection, policy enforcement, or binding authorization to execution.
