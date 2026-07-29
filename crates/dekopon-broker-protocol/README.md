# dekopon-broker-protocol

Versioned, length-delimited local broker messages and an unprivileged Unix-socket client.

The wire carries only capability inspection requests and untrusted `InvocationRequest` values. It has no principal, actor, policy, constraint, credential, or `AuthorizedInvocation` field. A broker server must derive `AuthenticatedContext` from operating-system peer credentials and trusted workload mapping.

Frames use a four-byte big-endian length followed by strict JSON. Reads, writes, connection setup, and complete frames have independent positive limits and deadlines; oversized lengths are rejected before allocation. Each client operation uses a fresh Unix connection and validates the exact protocol version and response variant.

This crate does not bind a socket or grant authority. `BrokerClient` can submit proposals and receive public capabilities/results only. Direct `dekopon-run` remains on its separate import-free component host unless an operator explicitly selects a future broker-client mode.
