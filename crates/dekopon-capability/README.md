# dekopon-capability

Capability descriptors and invocation typestates for Dekopon.

The API distinguishes model-proposed invocations from broker-authorized invocations. Rust visibility provides defense in depth; it does not replace broker process isolation, authenticated messages, or policy enforcement.
