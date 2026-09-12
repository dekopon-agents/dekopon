# dekopon-broker

Broker-owned authorization and execution core for Dekopon.

The crate binds an authenticated context supplied by a transport to an actor, asks a
`dekopon-policy` Cedar engine whether that context may act, binds an allow to the capability's
owner-authored constraint set, creates a single-use `AuthorizedInvocation`, executes it through
`dekopon-broker-host`, returns bounded public evidence, and emits a metadata-only audit record for
every decision.

Authorization and execution are separate by construction. Cedar decides *who may do what*; a
`ConstraintSet` decides *how narrowly the broker then does it* — provider route, trusted
effect/risk classification, optional symbolic credential, timeout, output ceiling, and
exact HTTP authority. Constraint sets are validated at startup against the loaded provider manifest,
the component host's independent ceilings, and the credential store, and no policy edit reaches
them. A capability with no constraint set is denied `unconstrained-capability` before Cedar is
consulted. `Leniency::Strict` refuses startup if any policy could ever permit such a capability;
`Leniency::Tolerant` makes that a `StartupWarning`. Leniency governs startup only — the
invocation-time refusal, the part that enforces anything, is identical in both modes.
*Committed direction:* removed ([non-goals](../../docs/design.md#non-goals)).

A typed public DRN is untrusted proposal data. The broker requires ordinary capability policy and a
separate exact `secret.use` decision, matches an owner-only `SecretUseBinding`, commits the
effective narrower scope into authorization and evidence, resolves one invocation snapshot through a
brokerd-owned `SecretResolver`, and passes only native credential material to the host. A constraint
set may instead bind a symbolic `credential:` resolved from a caller-supplied `CredentialStore`;
construction fails closed on unknown names, missing HTTP authority, or allowed hosts outside the
credential's destination binding. See [`../../docs/secrets.md`](../../docs/secrets.md).
*Committed direction:* `credential`/`credentialByAgent` bindings will be replaced by public DRNs,
preserving per-agent isolation, destination binding, and broker-owned refresh
([migration requirements](../../docs/design.md#legacy-credential-bindings)).

## Security boundary

`AuthenticatedContext` is trusted input from a deployment adapter; constructing it or an
`AuthorizationGate` authenticates nothing. A broker process must derive principal and actor from
peer credentials and trusted workload mapping, never from request payload fields. `dekopon-brokerd`
is the deployment adapter that does, from Unix peer credentials and owner-controlled configuration
alone.

An invocation identifier binds an attestation to the proposal it travels with and names the call in
the audit record. The broker suppresses no duplicate: a resubmitted identifier is evaluated and
executed again ([non-goals](../../docs/design.md#non-goals)). Authorization is non-cloneable and
consumed by provider execution.

Public results carry an inert decision ID, broker and policy reference, and digest evidence.
*Committed direction:* removed; the trace is the record
([design.md](../../docs/design.md#core-concepts)).

Audit records contain identities, routing and policy metadata — `policy_ids`, the policies that
determined the decision, and `policy_digest`, a fingerprint of the evaluated set — stable outcomes,
timings, output digests, and sanitized HTTP call metadata. They never contain invocation input,
provider output, paths, queries, headers, bodies, cookies, authorization values, or credentials.

The record is a structured `tracing` event on target `dekopon_broker::audit`, emitted inside the span
that made the decision: `broker.decision` for an allow or deny, `broker.execution` for a terminal
outcome, including an authorized failure before the provider ran
([fields](../../docs/observability.md#the-broker-audit-record)). It is emitted before the `AuditLog`
sink is offered the event, so a sink cannot suppress it. `TraceOnlyAuditLog` keeps nothing and is
the sink [`dekopon-brokerd`](../dekopon-brokerd/README.md#audit) runs. `InMemoryAuditLog` is a
bounded sink for tests and embedding: `records()` returns the events in append order, and a full log
refuses with `AuditError::Full`, its only failure. A refusal before the provider ran — of the
decision or of an authorized failure — means nothing executed; a refused terminal outcome is
reported through `BrokerError::unaudited_outcome`, because the effect may already have happened.

## Optional durable chat memory

Chat operations add canonical transport/channel/conversation authority to the subject mapping and
the `agent.prompt` gate. Owner configuration must grant both the subject namespace and an explicit
`chatScopes` breadth, and Cedar receives those scope fields. Reservation follows what the owner
declared: each of the three capabilities carries a `route` of `chatMemoryRecord`,
`chatMemoryRecent`, or `chatMemorySearch`, and the list, run, resolve, and invoke paths refuse
exactly those and every command word of the provider they name. No capability or provider spelling
reserves anything. Recent and search are visible only as an all-three surface; record is reachable
only through the dedicated typed post-acceptance operation.

Storage audit records replace raw identity, provider, and policy metadata with a domain-separated
scope commitment and content-free evidence. `authority-bound` continuity hashes only the
sorted effective capability/artifact/constraint/selected-credential/host/storage/memory surface plus
persisted random epochs, so a semantic A→B→A creates three generations. Provider and config
ordering, unrelated denied providers, enabled-agent ordering, policy formatting, and a principal
remap that leaves the canonical subject's effective surface unchanged do not rotate it. Explicit
`stable` preserves the namespace across semantic changes.
