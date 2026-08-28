# dekopon-broker

Broker-owned authorization and execution core for Dekopon.

A typed public DRN remains untrusted proposal data. The broker requires both ordinary capability
policy and a separate exact `secret.use` decision, matches an owner-only `SecretUseBinding`, commits
the effective narrower scope into authorization/evidence, resolves one invocation snapshot through
a brokerd-owned `SecretResolver`, and passes only native credential material to the host. Existing
implicit `CredentialStore` selection remains compatible. See [`../../docs/secrets.md`](../../docs/secrets.md).

This crate binds an authenticated context supplied by a transport to an actor, asks a `dekopon-policy` Cedar engine whether that context may act, binds an allow to the capability's owner-authored constraint set, creates a single-use `AuthorizedInvocation`, executes it through `dekopon-broker-host`, returns bounded public evidence, and records a hash-linked metadata-only audit chain.

Authorization and execution are separate by construction. Cedar decides *who may do what*; a `ConstraintSet` decides *how narrowly the broker then does it* — provider route, trusted effect/risk/idempotency classification, optional symbolic credential, timeout, output ceiling, and exact HTTP authority. Constraint sets are validated at startup against the loaded provider manifest, the component host's independent ceilings, and the credential store, and no policy edit can reach them. A capability with no constraint set is denied `unconstrained-capability` before Cedar is consulted. Under `Leniency::Strict` the broker also refuses to start if any policy could ever permit one; under `Leniency::Tolerant` that becomes a `StartupWarning` so a deployment can ship configuration anticipating a provider it has not dropped in yet. Leniency governs startup only — the invocation-time refusal, which is the part that enforces anything, is identical in both modes.

## Security boundary

`AuthenticatedContext` is trusted input from a deployment adapter. Constructing it or an `AuthorizationGate` does not authenticate anything. A broker process must derive the principal and actor from peer credentials and trusted workload mapping, never request payload fields; `dekopon-brokerd` is the deployment adapter that does, from Unix peer credentials and owner-controlled configuration alone.

Invocation IDs are reserved in a bounded replay ledger before policy evaluation, so repeated denied requests cannot later be reused for execution. Exhaustion fails closed; transport-level quotas must prevent an authenticated peer from consuming the ledger. A broker restart can seed the ledger from decision IDs reconstructed from a verified durable audit chain. Authorization is non-cloneable and consumed by provider execution. Public results carry an inert decision ID/broker/policy reference plus digest evidence. Audit records contain identities, routing and policy metadata — including `policy_ids`, the identifiers of the policies that determined the decision, and `policy_digest`, a fingerprint of the evaluated policy set — stable outcomes, timings, output digests, and sanitized HTTP call metadata. They never contain invocation input, provider output, paths, queries, headers, bodies, cookies, authorization values, or credentials.

The in-memory audit implementation is deterministic and bounded for tests. `FileAuditLog` creates or opens an exclusively writer-locked, single-link owner-only JSONL file without following symlinks, bounds every line and record count, verifies the complete chain before appending, flushes and synchronizes each record, poisons a handle after a failed write, rejects partial final records, and exposes a chain checkpoint, exact-prefix comparison, and verified replay IDs. `dekopon-brokerd` composes it with an atomic separately locked checkpoint file to detect valid-prefix rollback relative to retained local state. Authenticated transport and checkpoint persistence remain service layers; independent remote/signed anchoring is not implemented. A constraint set may bind a symbolic `credential:` resolved from a caller-supplied `CredentialStore`; construction fails closed on unknown names, missing HTTP authority, or allowed hosts outside the credential's destination binding.

## Optional durable chat memory

New chat operations add canonical transport/channel/conversation authority to the existing subject
mapping and `agent.prompt` gate. Owner configuration must grant both the subject namespace and an
explicit `chatScopes` breadth; Cedar receives those scope fields. What is reserved is what the owner
declared: each of the three capabilities carries a `route` of `chatMemoryRecord`, `chatMemoryRecent`,
or `chatMemorySearch`, and legacy list/resolve/invoke paths omit and refuse exactly those and every
command word of the provider they name — no capability or provider spelling reserves anything.
Recent/search are visible only as an
all-three surface; record is reachable only through the dedicated typed post-acceptance operation.
Storage audit records replace raw identities/provider/policy metadata with a domain-separated keyed
scope commitment and content-free evidence. `authority-bound` continuity uses only the sorted
effective capability/artifact/constraint/selected-credential/host/storage/memory surface and
persisted random epochs, so semantic A→B→A creates three generations. Provider/config ordering,
unrelated denied providers, enabled-agent ordering, policy formatting, and a principal remap that
leaves the canonical subject's effective surface unchanged do not rotate. Explicit `stable`
preserves the namespace across semantic changes.
