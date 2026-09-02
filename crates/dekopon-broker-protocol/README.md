# dekopon-broker-protocol

Versioned, length-delimited local broker messages and an unprivileged Unix-socket client.

There is one operation per verb — `capabilities`, `resolveCommand`, `invoke`, `recordDeliveredTurn`, and the two informational reports below. Whether a caller speaks as its own authenticated peer, on behalf of an external subject, or inside a bounded chat scope is one optional `Attestation` field on the operation rather than an operation of its own; `scope` distinguishes a chat claim from a subject-only one, and `invocation` binds a claim to the proposal it accompanies on exactly the two operations that carry one.

The authority-bearing half of the wire carries only capability inspection requests and untrusted `InvocationRequest` values. It has no principal, actor, policy, constraint, credential **value**, or `AuthorizedInvocation` field. An optional `secretUse` is an inert canonical public DRN plus native sink intent; possession grants nothing, and providers never receive it. A broker server must derive `AuthenticatedContext` from operating-system peer credentials and trusted workload mapping, then separately authorize and bind any DRN use.

`ResolveCommand` (`resolveCommand`) is the one authority-bearing operation deliberately **not** gated on the caller's grants. It rewrites one provider-declared shell command word and its arguments into a capability proposal by calling the declaring component's pure, import-free, fuel- and timeout-bounded `run-command` export (or the legacy `resolve-command`, when that is all it exports); a help page or usage error a `run-command` guest renders reaches this operation's caller as a decline carrying that text. What comes back is a proposal, authorized on exactly the path every other proposal takes, so a caller who rewrites a word they may not use receives a denial one step later having learned nothing they could not learn by naming the capability directly. A guest failure is the stable `provider-error` code with a deliberately opaque message; a provider that declines the argv is a usage error carrying the provider's own text.

Two additional operations let a mapped gateway attestor publish bounded informational state for `dekopon-webui`: a normalized catalog-agent inventory and provider-reported model-token deltas. They contain no instructions, prompts, answers, subjects, principals, credentials, policy, constraints, or authorization. A broker may display them and must never use them for identity, authorization, routing, execution, evidence, replay, or durable audit.

Frames use a four-byte big-endian length followed by strict JSON. Reads, writes, connection setup, and complete frames have independent positive limits and deadlines; oversized lengths are rejected before allocation, and an in-bound length is a claim the reader never pre-allocates against — payload buffers grow with the bytes that actually arrive, and a frame shorter than its prefix fails rather than decoding. One frame is one write. Each client operation uses a fresh Unix connection and validates the exact protocol version and response variant.

`ClientError` distinguishes the phase a framing failure belongs to, because the wire's `broker-unavailable` / `outcome-unaudited` split is worth nothing if a client-local timeout erases it. A request-phase failure delivered nothing and is safe to resubmit under a fresh invocation identifier; a response-phase failure delivered the complete request and could not read the answer, so a write may already have happened. `ClientError::may_have_executed` answers that question for both cases and for the broker's own `outcome-unaudited` code; a caller that writes must surface it as non-retryable rather than resubmitting.

This crate depends only on wire/domain/provider-metadata types, not `dekopon-broker`, `dekopon-broker-host`, or the native HTTP engine. It does not bind a socket or grant authority. `BrokerClient` can submit proposals and receive public capabilities/results only. Direct `dekopon-run` remains on its separate import-free component host. Explicit `dekopon-run broker` commands use this client, load no component, and gain no effect authority. `dekopond` is the other consumer, reaching the broker through `dekopon-agent`, carrying the attested on-behalf-of claim this protocol defines, and best-effort publishing the explicitly non-authoritative inventory/accounting reports. Reporting failures do not change a session result.

A chat claim carries a fully redacted bounded scope over configured transport ID, transport kind,
canonical channel, and canonical conversation. Bounded string deserializers reject an oversized
field while decoding, and `Attestation` renders as `[REDACTED]` whatever shape it holds.
`RecordDeliveredTurn` carries a tagged service-specific `DeliveryIdentity` whose Slack
channel/timestamp, Discord channel/snowflake, Telegram chat/topic/message, WhatsApp
WABA/phone-number/canonical message ID, or local transport/conversation/boot nonce is checked
against that attested scope. It is a separate typed operation; `invoke` cannot reach hidden
recording under any attestation. `ChatMemorySurface` is
present only when the broker freshly authorizes the complete surface. `PROTOCOL_VERSION` is
`dekopon.dev/broker/v1alpha2`; both envelopes are strict-decoded, so a broker and a client from
different protocol versions refuse each other's first frame as `invalid-request` in either
direction rather than misinterpreting it. Pre-execution storage setup failures
retain the stable public codes `storage-quota`, `storage-busy`, `storage-timeout`,
`storage-corrupt`, and `storage-io`; `outcome-unaudited` remains reserved for a durable point that
may already have been crossed.
