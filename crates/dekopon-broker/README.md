# dekopon-broker

Broker-owned authorization and execution core for Dekopon.

The crate binds an authenticated context supplied by a transport to an actor, asks a
`dekopon-policy` Cedar engine whether that context may act, binds an allow to the capability's
owner-authored constraint set, creates a single-use `AuthorizedInvocation`, executes it through
`dekopon-broker-host`, returns bounded public evidence, and appends metadata-only audit records.

Authorization and execution are separate by construction. Cedar decides *who may do what*; a
`ConstraintSet` decides *how narrowly the broker then does it* — provider route, trusted
effect/risk/idempotency classification, optional symbolic credential, timeout, output ceiling, and
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

Invocation IDs are reserved in a bounded replay ledger before policy evaluation, so repeated denied
requests cannot later be reused for execution. Exhaustion fails closed; transport-level quotas must
prevent an authenticated peer from consuming the ledger. A restart starts an empty process-local
ledger, and the audit file supplies no replay identities. Authorization is non-cloneable and
consumed by provider execution.

Public results carry an inert decision ID, broker and policy reference, and digest evidence.
*Committed direction:* removed; the trace is the record
([design.md](../../docs/design.md#core-concepts)).

Audit records contain identities, routing and policy metadata — `policy_ids`, the policies that
determined the decision, and `policy_digest`, a fingerprint of the evaluated set — stable outcomes,
timings, output digests, and sanitized HTTP call metadata. They never contain invocation input,
provider output, paths, queries, headers, bodies, cookies, authorization values, or credentials.

Two sinks implement the record: a bounded in-memory log for tests and embedding, and
`FileAuditLog`, whose file rules, ordinal counting, and append durability are the
[`dekopon-brokerd` audit contract](../dekopon-brokerd/README.md#audit). A failed or cancelled append
poisons the handle. *Committed direction:* opt-in sink, off by default; audit is a log record in
the trace ([non-goals](../../docs/design.md#non-goals)).

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
keyed scope commitment and content-free evidence. `authority-bound` continuity hashes only the
sorted effective capability/artifact/constraint/selected-credential/host/storage/memory surface plus
persisted random epochs, so a semantic A→B→A creates three generations. Provider and config
ordering, unrelated denied providers, enabled-agent ordering, policy formatting, and a principal
remap that leaves the canonical subject's effective surface unchanged do not rotate it. Explicit
`stable` preserves the namespace across semantic changes.
