# dekopon-broker-protocol

Versioned, length-delimited local broker messages and an unprivileged Unix-socket client.

There is one operation per verb — `capabilities`, `runCommand`, `invoke`, and
`recordDeliveredTurn`. Whether a caller speaks as its own authenticated peer, on behalf of an external
subject, or inside a bounded chat scope is one optional `Attestation` field on the operation rather
than an operation of its own; `scope` distinguishes a chat claim from a subject-only one, and
`invocation` binds a claim to the proposal it accompanies on exactly the two operations that carry
one.

The authority-bearing half of the wire carries only capability inspection requests and untrusted
`InvocationRequest` values. It has no principal, actor, policy, constraint, credential **value**, or
`AuthorizedInvocation` field. An optional `secretUse` is an inert canonical public DRN plus native
sink intent; possession grants nothing, and providers never receive it. A broker server must derive
`AuthenticatedContext` from operating-system peer credentials and trusted workload mapping, then
separately authorize and bind any DRN use.

`RunCommand` (`runCommand`) is the one authority-bearing operation that is not gated on the caller's
grants. It runs one provider-declared shell command word, its arguments, and the optional value the
script piped into it (`stdin`) through the declaring component's pure, import-free, fuel- and
timeout-bounded `run-command` export, and answers with the guest's own `CommandRunOutcome`: a
proposal to submit, text the guest rendered together with the exit status it chose, or a decline
carrying the guest's stable code and message. A proposal is authorized on exactly the path every
other proposal takes, so a caller who runs a word they may not use receives a denial one step later
having learned nothing they could not learn by naming the capability directly; rendered text
authorizes nothing. The piped value is bounded twice: by the frame ceiling on the client, where an
oversized value fails in the request phase before a byte is written, and by the broker host's input
bound before a store exists. A guest failure is the stable `provider-error` code with an opaque
message.

Unix clients accept server-owned, single-link `0600` sockets and shared IPC `0660` sockets, and
inspect the parent directory in both cases: it must be a server-owned, non-symlink directory,
private or group-traversable, with no group writes and no access for others. A shared socket must
additionally carry that parent's GID and its group-traversal bit. `secure_socket_parent`,
`secure_socket`, and `ipc_socket_mode` are the one definition of those rules; `dekopon-brokerd`
calls them before binding, so a client trusts exactly the sockets the broker would bind. Filesystem
metadata is not server authentication: the connected peer UID must also match the configured server
UID before any request bytes are sent. Group access never supplies caller identity; the broker maps
the actual peer UID. See the broker's
[IPC directory contract](../dekopon-brokerd/README.md#ipc-directory-and-distinct-peer-uids).

Frames use a four-byte big-endian length followed by strict JSON. Reads, writes, connection setup,
and complete frames have independent positive limits and deadlines; oversized lengths are rejected
before allocation, and an in-bound length is a claim the reader never pre-allocates against —
payload buffers grow with the bytes that actually arrive, and a frame shorter than its prefix fails
rather than decoding. One frame is one write. Each client operation uses a fresh Unix connection and
validates the exact protocol version and response variant.

`ClientError` distinguishes the phase a framing failure belongs to, because the wire's
`broker-unavailable` / `outcome-unaudited` split is worth nothing if a client-local timeout erases
it. A request-phase failure delivered nothing and is safe to resubmit under a fresh invocation
identifier; a response-phase failure delivered the complete request and could not read the answer,
so a write may already have happened. `ClientError::may_have_executed` answers that question for
both cases and for the broker's own `outcome-unaudited` code; a caller that writes must surface it
as non-retryable rather than resubmitting.

This crate depends only on wire, domain, and provider-metadata types, not `dekopon-broker`,
`dekopon-broker-host`, or the native HTTP engine. It binds no socket and grants no authority.
`BrokerClient` can submit proposals and receive public capabilities and results only. `dekopond` is
a consumer, reaching the broker through `dekopon-agent`, carrying the attested on-behalf-of claim
this protocol defines.

A chat claim carries a fully redacted bounded scope over configured transport ID, transport kind,
canonical channel, and canonical conversation. Bounded string deserializers reject an oversized
field while decoding, and `Attestation` renders as `[REDACTED]` whatever shape it holds.
`RecordDeliveredTurn` carries a tagged service-specific `DeliveryIdentity` whose Slack
channel/timestamp, Discord channel/snowflake, Telegram chat/topic/message, WhatsApp
WABA/phone-number/canonical message ID, or local transport/conversation/boot nonce is checked
against that attested scope. It is a separate typed operation; `invoke` cannot reach hidden
recording under any attestation. `ChatMemorySurface` is present only when the broker freshly
authorizes the complete surface. `PROTOCOL_VERSION` is `dekopon.dev/broker/v1alpha2`; both envelopes
are strict-decoded, so a broker and a client from different protocol versions refuse each other's
first frame as `invalid-request` in either direction rather than misinterpreting it. Pre-execution
storage setup failures carry the stable public codes `storage-quota`, `storage-busy`,
`storage-timeout`, `storage-corrupt`, and `storage-io`; `outcome-unaudited` is reserved for a
durable point that may already have been crossed.

Shared socket discovery (`BrokerSocketDiscovery`) resolves an explicit path, then
`DEKOPON_BROKER_SOCKET`, then `$XDG_RUNTIME_DIR/dekopon/broker.sock`, then
`$HOME/.local/run/dekopon/broker.sock`. No applicable tier is a caller-owned error. Paths are not
probed for existence: the tightest selected path remains authoritative while a daemon is stopped.
Discovery never replaces socket metadata or connected server-UID validation. See the
[current local process boundary](../../docs/security-model.md#current-local-process-boundary) for
distinct daemon UIDs and protected IPC; the local chat socket is owner-only.

## Attestation shape

When an `Attestation` accompanies `invoke` or `recordDeliveredTurn`, its `invocation` must equal the
accompanying proposal's identifier; on every other operation it must be absent. A malformed or
mismatched claim is rejected as `invalid-request` before anything is authorized, accounted, or
audited. `recordDeliveredTurn` requires a chat claim; the other operations accept a subject-only
claim, a chat claim, or none.

An attestor whose grant has no `chatScopes` entries keeps ordinary attested authority for an allowed
subject even when its claim carries a scope. That context has no trusted chat scope, so durable
memory is structurally unavailable. Once any `chatScopes` entry is authored, a chat claim must
satisfy the service-specific canonical checks and an exact matching grant. Claim shape itself grants
nothing.

## Command execution refusals

Everything that is not the guest's own answer collapses into one opaque reply. A word no loaded
provider declares, an input past the host bound, a guest that traps or reaches for a host import,
and an answer the broker cannot decode all return the stable `provider-error` code with a fixed
message; the guest's own failure text is provider-controlled and never reaches a caller through this
path. The broker logs `command.resolve.failed` naming the word, so an operator can tell the cases
apart from the audit stream a caller cannot read. A provider that simply *declines* the arguments is
not a failure at all: that is a usage error, and the provider's own message travels back for the
model to read.

Reserved words are unreachable through this path, and what reserves them is the deployment's own
`constraintSets`: every word belonging to the provider a chat-memory `route:` names is refused here
*before the guest runs*, so a reserved provider renders not even its help page for a caller without
the surface, and so is any proposal that lands on a chat-memory-routed capability. Reservation
follows what an operator declared rather than how a provider or capability happens to be spelled, so
hidden chat recording cannot be reached by a command word any more than by generic invocation. A
chat claim lifts the reservation on exactly the memory *retrieval* words, and only for a session
whose three memory grants are effective; recording stays unreachable from every word.

An `attestation` does not gate the run either, but it is a claim, and a claim the broker refuses
buys nothing: the word answers as an undeclared word does, because naming it would disclose the
surface the refusal withheld.

## Version and compatibility

Upgrade `dekopond` and `dekopon-brokerd` together. The alpha protocol has no cross-release
compatibility promise or negotiation. Start the broker first and stop it last: the gateway probes
capabilities before connecting transports. Unknown operation tags fail strict decoding as
`invalid-request`; no compatibility sink accepts them. The broker's `--http-bind` argument and chart
`broker.httpBind` value are refused, including an empty chart value.

## Failure codes

A failure response carries a stable code and a bounded message. The code is the contract; the
message is human-facing and may change. Codes are exported as constants from
`dekopon-broker-protocol` so clients need not hardcode strings.

| Code | Meaning | Safe to resubmit? |
| --- | --- | --- |
| `unauthenticated` | The connected peer UID is not mapped by broker policy. | Not until the peer is mapped. |
| `invalid-request` | The request frame could not be decoded, or an attestation was malformed or mismatched to its operation or proposal. | Yes, once corrected. |
| `broker-unavailable` | The broker could not complete the request and **no provider work began**. | Yes, under a fresh invocation identifier. |
| `capacity-exhausted` | A bounded broker resource — the in-memory audit log of an embedding that serves `BrokerServer` over one — is full and does not evict. `dekopon-brokerd`'s own audit sink never fills. No provider work began. | Safe, and futile: it fails identically until an operator raises the bound. **Do not retry.** |
| `provider-error` | A `runCommand` run did not produce an answer: no loaded provider declares the word, the argv plus piped value exceeded the host's input bound, the guest failed, or its answer would not decode. **No invocation existed and nothing executed.** | Not without changing the word, its arguments, or the piped value; an identical retry fails identically. |
| `outcome-unaudited` | Provider work may already have completed and the broker did not record its outcome. | **No.** The external effect may have taken place. |
| `storage-quota`, `storage-busy`, `storage-timeout`, `storage-corrupt`, `storage-io` | Broker-owned namespace/grant setup failed before provider execution. A `storage-corrupt` whose message says the storage was reset has already moved that conversation to fresh, empty storage. | Yes under a fresh identifier after correcting or reconciling the storage condition; after a reset, immediately. |

`outcome-unaudited` separates "nothing happened" from "something may have happened and nothing
recorded it". It is emitted only for failures raised after execution began: terminal evidence that
fails to hash, or an embedding's bounded audit sink refusing the terminal record, which
`dekopon-brokerd`'s own sink never does. The server logs `broker_outcome_unaudited` with the
invocation identifier, so the invocation needing manual reconciliation is identifiable without
correlating client-side state. A denied or failed *invocation* is not a failure response at all: it
returns a normal result carrying its outcome and decision linkage.

Only on a chat-memory-routed capability may a provider-reported failure retain one of
`memory-corrupt`, `result-too-large`, `dedup-conflict`, or `dedup-capacity`. All other
provider-reported failures use `provider-failure`, and arbitrary provider messages remain opaque.
This routed provider-code allowlist is separate from the native pre-execution storage setup failures
above.

`capacity-exhausted` separates an exhausted bounded embedding in-memory audit log from a momentary
outage. It does not evict during its lifetime, so clients must not retry automatically. The server
logs `broker_capacity_exhausted`; an operator must address capacity.

A refused or unresolvable DRN is never a failure response: refusal is a normal `Denied` invocation
(`secret-denied`) and a post-authorization source failure is a normal `Failed` invocation
(`secret-resolution`) with a terminal audit record and zero HTTP calls, so retry is safe but useful
only after the source condition changes.
[Secret policies](../../docs/secrets.md#two-independent-policies) defines which conditions share
`secret-denied`, and [Resolution and rotation](../../docs/secrets.md#resolution-and-rotation)
defines the source-failure conditions.

A failure response is not the only way to reach that state. Nothing ties a client's `io_timeout` to
broker-side execution deadlines, so a client whose response read fails is in the same position: the
complete request frame was delivered and the outcome is unknown to it. A caller submitting a write
must map `ClientError::may_have_executed` to a non-retryable result; `dekopon-agent` reports it to a
script as `denied` (exit `126`) rather than as a generic failure, because the broker suppresses no
duplicate and a resubmission repeats the effect.
