# dekopon-broker

Broker-owned authorization and execution core for Dekopon.

This crate binds an authenticated context supplied by a transport to an actor, applies exact deny-by-default policy, creates a single-use `AuthorizedInvocation`, executes it through `dekopon-broker-host`, returns bounded public evidence, and records a hash-linked metadata-only audit chain. Policy rules name an exact principal, actor, capability, provider, effect, risk, idempotency class, and execution constraints; there are no wildcard grants.

## Security boundary

`AuthenticatedContext` is trusted input from a deployment adapter. Constructing it or an `AuthorizationGate` does not authenticate anything. The eventual broker process must derive the principal and actor from peer credentials and trusted workload mapping, never request payload fields.

Invocation IDs are reserved in a bounded replay ledger before policy evaluation, so repeated denied requests cannot later be reused for execution. Exhaustion fails closed; transport-level quotas must prevent an authenticated peer from consuming the ledger. A broker restart can seed the ledger from decision IDs reconstructed from a verified durable audit chain. Authorization is non-cloneable and consumed by provider execution. Public results carry an inert decision ID/broker/policy reference plus digest evidence. Audit records contain identities, routing and policy metadata, stable outcomes, timings, output digests, and sanitized HTTP call metadata. They never contain invocation input, provider output, paths, queries, headers, bodies, cookies, authorization values, or credentials.

The in-memory audit implementation is deterministic and bounded for tests. `FileAuditLog` creates or opens an exclusively writer-locked, single-link owner-only JSONL file without following symlinks, bounds every line and record count, verifies the complete chain before appending, flushes and synchronizes each record, poisons a handle after a failed write, rejects partial final records, and exposes a chain checkpoint plus verified replay IDs. A known external checkpoint is still required to detect valid-prefix truncation. Authenticated transport, checkpoint anchoring, credentials, and an executable service remain separate layers.
