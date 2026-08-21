# dekopon-broker-protocol

Versioned, length-delimited local broker messages and an unprivileged Unix-socket client.

The authority-bearing half of the wire carries only capability inspection requests and untrusted `InvocationRequest` values. It has no principal, actor, policy, constraint, credential, or `AuthorizedInvocation` field. A broker server must derive `AuthenticatedContext` from operating-system peer credentials and trusted workload mapping.

`ResolveCommand` (`resolveCommand`) is the one authority-bearing operation deliberately **not** gated on the caller's grants. It rewrites one provider-declared shell command word and its arguments into a capability proposal by calling the declaring component's pure, import-free, fuel- and timeout-bounded `resolve-command` export. What comes back is a proposal, authorized on exactly the path every other proposal takes, so a caller who rewrites a word they may not use receives a denial one step later having learned nothing they could not learn by naming the capability directly. A guest failure is the stable `provider-error` code with a deliberately opaque message; a provider that declines the argv is a usage error carrying the provider's own text.

Two additional operations let a mapped gateway attestor publish bounded informational state for `dekopon-webui`: a normalized catalog-agent inventory and provider-reported model-token deltas. They contain no instructions, prompts, answers, subjects, principals, credentials, policy, constraints, or authorization. A broker may display them and must never use them for identity, authorization, routing, execution, evidence, replay, or durable audit.

Frames use a four-byte big-endian length followed by strict JSON. Reads, writes, connection setup, and complete frames have independent positive limits and deadlines; oversized lengths are rejected before allocation. Each client operation uses a fresh Unix connection and validates the exact protocol version and response variant.

This crate depends only on wire/domain/provider-metadata types, not `dekopon-broker`, `dekopon-broker-host`, or the native HTTP engine. It does not bind a socket or grant authority. `BrokerClient` can submit proposals and receive public capabilities/results only. Direct `dekopon-run` remains on its separate import-free component host. Explicit `dekopon-run broker` commands use this client, load no component, and gain no effect authority. `dekopond` is the other consumer, reaching the broker through `dekopon-agent`, carrying the attested on-behalf-of variant this protocol defines, and best-effort publishing the explicitly non-authoritative inventory/accounting reports. Reporting failures do not change a session result.

Chat-scoped operations carry a fully redacted bounded claim over configured transport ID, transport
kind, canonical channel, and canonical conversation. Bounded string deserializers reject an
oversized field while decoding. `ChatAttestation` binds that scope and the subject/agent to one
invocation. `RecordDeliveredTurnForChat` carries a tagged service-specific `DeliveryIdentity` whose
Slack channel/timestamp, Discord channel/snowflake, Telegram chat/topic/message, or local
transport/conversation/boot nonce is checked against that attested scope. It is a separate typed
operation; generic invocation cannot reach hidden recording. `ChatMemorySurface` is present only when the
broker freshly authorizes the complete surface. Older strict brokers reject these new operation
tags as `invalid-request` rather than misinterpreting them. Pre-execution storage setup failures
retain the stable public codes `storage-quota`, `storage-busy`, `storage-timeout`,
`storage-corrupt`, and `storage-io`; `outcome-unaudited` remains reserved for a durable point that
may already have been crossed.
