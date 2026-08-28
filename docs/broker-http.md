# Broker-mediated HTTP providers

This document records the implemented privileged host, local Unix broker, credential resolution, and Cedar authorization, plus **committed direction** for stronger deployment transports and independent checkpoint anchoring. Status is called out explicitly because local UID authentication is not a production multi-tenant boundary.

## Current foundation

The immutable `dekopon:http@1.0.0` WIT package, the `dekopon-provider-http` Rust guest facade, SDK support for caller-generated provider worlds, the statically linked `dekopon-http-host` native engine, and the async `dekopon-broker-host` component adapter are current. The checked-in HTTP probe proves both direct-host rejection and constrained broker-host execution against ephemeral loopback servers. For HTTP operations, the broker host compiles components once, creates fresh bounded stores, links the project-owned HTTP import, consumes `AuthorizedInvocation`, applies exact HTTP constraints, and emits sanitized call metadata. This tree additionally links the independent project-owned storage package only under the exact grant described below.

`dekopon-broker` now binds a separately supplied authenticated context, asks a `dekopon-policy` Cedar engine whether it may act, validates trusted metadata and constraints at startup, rejects invocation-ID reuse across verified durable history and the current process, creates and consumes single-use authorization, returns an inert decision reference plus digest evidence, and appends redacted events to a bounded verifiable in-memory or durable JSONL hash chain. Its integration tests prove deny-before-execution and ensure input, output, URL path/query, headers, and bodies do not enter audit records.

The versioned local wire format, explicit `dekopon-run broker` Unix client commands, `dekopon-brokerd` process, JSONPlaceholder demonstration provider, and atomic local audit checkpoint are also current. Frames are length-delimited and deadline-bounded; invocation payloads contain no identity/authority fields; clients verify private socket ownership plus server peer UID; and the listener maps connected peer UID through strict owner-controlled configuration before invoking the core. Broker-owned destination-bound credential resolution is current, per capability and optionally per acting agent. Public logical DRNs and the owner-only private secret map are also current: an exact typed proposal passes a separate `secret.use` decision and native use binding before one source snapshot resolves. Broker-side attested identity remains owner-configured grants, subject-to-principal mappings, and policy conditioned on `context.via`. The agent half of orchestration integration is current too: `dekopond` drives bounded sessions whose capability calls become attested proposals on this protocol ([`dekopond.md`](dekopond.md)). Independent checkpoint retention/signing, a dedicated gateway UID, and operator-CLI integration remain committed direction.

## Decision

Dekopon will expose a project-owned, buffered WebAssembly Component Model interface named `dekopon:http@1.0.0`. Provider components may import that interface, but an import is only a structural requirement. It is not authority to contact a destination.

The native HTTP implementation belongs to `dekopon-brokerd` and is statically linked into the broker. Provider-side Rust bindings are compiled into each guest component. Dekopon will not dynamically load Rust `dylib` plugins or download an implementation in response to an untrusted component import.

The existing immediate mode remains distinct:

- direct `dekopon-run` providers remain read-only and import-free;
- broker-backed `dekopon-run` operations send proposals to a separately running broker;
- `dekopon-run` never links `dekopon:http`, resolves provider credentials, or constructs an `AuthorizedInvocation`;
- the broker authenticates the caller, authorizes the proposal, resolves supported imports, executes the provider, and records the result.

This preserves the rule that orchestration does not gain provider authority merely because it can ask for a tool call.

## Process and authority flow

```text
model or direct dekopon-run request
              |
              | untrusted capability + arguments
              v
      ProposedInvocation request
              |
              | authenticated local transport
              v
        dekopon-brokerd
          authenticate peer
          reject replay
          resolve trusted capability metadata
          evaluate operator policy
          create and consume AuthorizedInvocation
          attach HTTP and execution constraints
              |
              | broker-owned Wasmtime linker
              v
        provider component
          import dekopon:http/client@1.0.0
              |
              | bounded native host call
              v
          external endpoint
              |
              v
       result + evidence + audit
```

Explicit direct-runner proposal submission and the flow from authenticated socket acceptance downward are current, and so is automatic agent orchestration: a `dekopond` session's capability calls arrive here as attested proposals with no human in the loop. Operator-CLI integration remains future work. External-write capabilities require this separate broker process even when a demonstration endpoint does not persist writes. The JSONPlaceholder component exposes `jsonplaceholder.posts.get` as read-only/idempotent and `jsonplaceholder.posts.create` as external-write/non-idempotent. It defaults to the exact production HTTPS origin, accepts only literal loopback HTTP overrides for deterministic tests, validates bounded typed inputs and responses, and still relies on independent exact broker authority/method grants. Automated tests never contact the public service. A provider manifest remains untrusted input; trusted policy and capability configuration determine whether an operation is authorized.

## Local broker protocol

The current protocol and service use a local Unix-domain socket. `dekopon-brokerd` owns a path under an owner-only directory, rejects unsafe replacement and live listeners, creates mode `0600`, authenticates the connected stream from operating-system peer credentials, and maps that UID to one exact configured principal/actor. Payload fields have no principal or actor slot and cannot override authenticated identity. Because the socket is owner-only, this is one UID trust domain rather than per-process attestation.

Each invocation request carries a unique invocation identifier, trace identifier, capability identifier, bounded JSON input, and protocol version. The broker rejects duplicate invocation identifiers across verified durable history and current state. Requests and responses use four-byte length-delimited, size-bounded strict JSON with one complete-frame deadline, so a peer cannot force unbounded buffering or hold a partial frame indefinitely.

The protocol exposes only the operations needed by a broker client — **one operation per verb**:

- `capabilities` inspects what this context may use;
- `resolveCommand` rewrites one provider-declared shell command word into a capability proposal;
- `invoke` submits one invocation proposal;
- `recordDeliveredTurn` submits hidden post-acceptance recording, and is the only way to reach it;
- `publishAgentInventory` and `publishModelUsage` let a mapped attestor report a bounded
  informational catalog inventory and model-token delta for the process-local web UI; and
- every operation receives a denied, succeeded, or failed result with bounded public evidence
  metadata, or a stable failure code.

Whether a caller speaks as its own authenticated peer, on behalf of an external subject, or inside a
bounded chat scope is **`attestation`, an optional field on the operation** — not an operation of its
own. One shape:

```json
{"subject": "slack.t0123abc.u9xyz", "agent": "reviewer",
 "scope": {"transport": "workspace-slack", "kind": "slack", "channel": "…", "conversation": "…"},
 "invocation": "invoke-…"}
```

`scope` is absent for a subject-only claim and present for a chat claim, which is the only claim that
can reach the durable-memory surface and the only one an owner-authored `chatScopes` grant applies
to. `invocation` is present exactly for the operations that carry a proposal — `invoke` and
`recordDeliveredTurn` — where it must equal that proposal's identifier. `recordDeliveredTurn`
requires a chat claim; every other operation accepts any shape or none.

The informational operations are deliberately outside the authority flow. Their payloads contain no prompt, subject, principal, instruction, credential, policy, constraint, or authorization; they grant nothing, produce no provider effect, and are absent from durable authorization audit. A gateway can misreport dashboard state and cannot use that state to influence a decision.

`AuthorizedInvocation` is never accepted from or returned to the client. It is created and consumed inside the broker process.

### `resolveCommand` is deliberately ungated

`resolveCommand` is the one operation that is **not** gated on the caller's grants, and the only one that runs guest code on request without an authorization decision first. A provider may declare command words in its manifest — `memory`, `gh` — so a model can write `memory recent --last 5` instead of assembling a capability input by hand. The broker selects the declaring provider by the word, calls that component's `resolve-command` export with the remaining arguments, and returns what it produces.

The reasoning is that the export is a **pure rewrite** and what it returns is a *proposal*. It runs inside the declaring component with no imports, bounded by the same fuel and timeout as any other guest call, and the proposal it produces is authorized on exactly the path a directly written capability call takes: constraint-set lookup, Cedar evaluation, credential injection at the native HTTP boundary. Gating the rewrite would add a principal check to a function that grants nothing. A caller who rewrites a word for a capability they may not use receives a denial one step later, having learned nothing they could not learn by naming the capability directly — and the word list a session is shown is already filtered by policy, so an ungranted principal is not handed the vocabulary in the first place.

Everything that is not a rewrite collapses into one opaque answer. A word no loaded provider declares, a guest that traps or reaches for a host import, and a resolution the broker cannot decode all return the stable `provider-error` code with a fixed message; the guest's own failure text is provider-controlled and never reaches a caller through this path. The broker logs `command.resolve.failed` naming the word, so an operator can tell the cases apart from the audit stream that a caller deliberately cannot. A provider that simply *declines* the arguments is not a failure at all: that is a usage error, and the provider's own message travels back for the model to read.

Reserved words are unreachable through this path, and what reserves them is the deployment's own `constraintSets`: every word belonging to the provider a chat-memory `route:` names is refused here, and so is any resolution that lands on a chat-memory-routed capability. Reservation therefore follows what an operator declared rather than how a provider or capability happens to be spelled, so hidden chat recording cannot be reached by rewrite any more than by generic invocation. A chat claim lifts the reservation on exactly the memory *retrieval* words, and only for a session whose three memory grants are effective; recording stays unreachable from every word.

An `attestation` does not gate the rewrite either, but it is still a claim, and a claim the broker refuses buys nothing: the word answers as an undeclared word does, because naming it would disclose the surface the refusal withheld.

### Attested on-behalf-of operations

An `attestation` carries a canonical external subject and an agent identity. It carries no principal, because the subject-to-principal mapping is owner-controlled broker state; a peer states *which authenticated external identity it is relaying*, and the broker decides who that is. It is honored only for peers whose configuration grants attestor authority over the subject's namespace, and a chat claim additionally needs a matching owner-authored `chatScopes` grant. An attestor whose grant names no `chatScopes` at all keeps working: a chat claim from it derives the legacy attested context with no trusted chat scope, which makes the durable-memory surface structurally unavailable rather than broken.

It travels as its own structure rather than as fields on the invocation, so the invocation payload stays identity-free whether or not one accompanies it. On `invoke` and `recordDeliveredTurn` its `invocation` must equal the accompanying proposal's identifier, and on every other operation it must be absent. They already travel in one frame, so this is defense in depth against a future refactor that separates them; a disagreement is a protocol error (`invalid-request`) rather than a policy decision, and nothing is authorized or accounted.

The two refusals differ in kind. A refused attestation on `invoke` is a normal invocation response carrying a `Denied` outcome and a stable reason, because it is a decision the broker made and durably recorded. Which reason depends on the claim's shape. A subject-only claim answers with the class itself — `attestation-denied` for a missing or out-of-scope grant, `unmapped-subject` for a subject no mapping names, `agent-denied` (or `policy-error`) for a principal policy does not let drive this agent. A chat-scoped claim answers with the single reason `chat-attestation-denied` for all four, on `invoke` and `recordDeliveredTurn` alike, because a session peer able to separate them would learn from its own denials which subjects the directory maps and which agents its principal may drive. The durable decision record keeps the real class and the policies that determined it either way. A refused `capabilities` is an `unauthenticated` failure response instead: there is no invocation to decide about, and answering with an empty list would tell an ungranted peer whether the subject is mapped. An unattested `capabilities` is never refused; it is the peer's own listing. The refusal class reaches the operator through `broker_capabilities_refused` on the broker's own side of the socket, never through the wire answer.

The `operation` tag is still strict-decoded, so an operation a broker does not know is a clean `invalid-request` rather than a misread proposal. It is no longer the *shape* seam it used to be, because shape is now a field; the envelope's `apiVersion` is.

### Version and compatibility

**Status: current.** `PROTOCOL_VERSION` is the single constant `dekopon.dev/broker/v1alpha2`, carried as `apiVersion` on every request and response envelope. There is one variant, so there is no negotiation: both envelopes are strict-decoded and any other string fails to deserialize. A request the broker cannot decode is answered `invalid-request` and the connection is closed; a response the client cannot decode is a client-side protocol error, which for a submitted invocation means the outcome is unknown to that client rather than known not to have happened.

**`dekopond`, `dekopon-run`, and `dekopon-brokerd` must be upgraded together.** They are separately installable — Homebrew, crates.io, release archives, the container image, and the chart with its own `image.tag` — so a mixed set is a normal deployment mistake rather than a hypothetical one, and the alpha protocol has no compatibility promise across releases. 0.5.0 changed it for policy-filtered command words and command resolution and required exactly this lockstep. `v1alpha2` changed it again, collapsing the per-attestation-shape operations into one operation per verb. The chart and the container image ship all four executables from one release for the same reason.

A mismatch is now loud in **both** directions, which is the whole reason the version moved rather than the operation tags being aliased:

- **A newer client against an older broker** sends `apiVersion: dekopon.dev/broker/v1alpha2`, which an older broker strict-decodes into `invalid-request`. Nothing is authorized, accounted, or audited.
- **An older client against a newer broker** sends `v1alpha1` and gets the same `invalid-request` on its first frame. Under `v1alpha1` this direction failed on the *response* instead — response variants are `deny_unknown_fields`, so a field added to a response an old client already understood made that response undecodable, and for a submitted proposal that is an unknown outcome rather than a refusal. Failing at the envelope moves that failure to before anything runs.

Retiring the old `operation` tags outright, rather than keeping them as deprecated aliases for a cycle, is the same decision: an alias would have carried the old *field shapes* too, which is exactly the multiplication the collapse removed, and a mixed pair would then have half-worked instead of refusing.

Restart order follows from the gateway's startup probe rather than from the protocol: `dekopond` asks the broker for capabilities once before connecting any transport and exits non-zero if the broker does not answer. Start the broker first and stop it last. [`upgrading.md`](upgrading.md) records the per-release steps.

### Failure codes

A failure response carries a stable code and a bounded message. The code is the contract; the message is human-facing and may change. Codes are exported as constants from `dekopon-broker-protocol` so clients need not hardcode strings.

| Code | Meaning | Safe to resubmit? |
| --- | --- | --- |
| `unauthenticated` | The connected peer UID is not mapped by broker policy. | Not until the peer is mapped. |
| `invalid-request` | The request frame could not be decoded. | Yes, once corrected. |
| `broker-unavailable` | The broker could not complete the request and **no provider work began**. | Yes, under a fresh invocation identifier. |
| `capacity-exhausted` | A bounded broker resource — the replay ledger or the audit log — is full. No provider work began. | Safe, and futile: it fails identically until an operator raises the bound. **Do not retry.** |
| `provider-error` | A `resolveCommand` rewrite did not produce a resolution: no loaded provider declares the word, the guest failed, or its answer would not decode. **No invocation existed and nothing executed.** | Not without changing the word or its arguments; an identical retry fails identically. |
| `outcome-unaudited` | Provider work may already have completed and the broker did not record its outcome. | **No.** The external effect may have taken place. |
| `storage-quota`, `storage-busy`, `storage-timeout`, `storage-corrupt`, `storage-io` | Broker-owned namespace/grant setup failed before provider execution. | Yes under a fresh identifier after correcting or reconciling the storage condition. |

`outcome-unaudited` is the durable-state signal that separates "nothing happened" from "something may have happened and nothing recorded it". It is emitted only for failures raised after execution began — a failed terminal audit append, or a failure to hash terminal evidence. A denied or failed *invocation* is not a failure response at all: it returns a normal result carrying its outcome and decision linkage.

The server logs `broker_outcome_unaudited` with the invocation identifier for exactly this case, so the invocation needing manual reconciliation is identifiable without correlating client-side state.

`capacity-exhausted` separates a permanent exhaustion from a momentary one. `broker-unavailable` invites a retry under a fresh invocation identifier; the replay ledger and the audit log are conditions under which that retry can never succeed. Neither structure evicts or rotates, and restart does not clear the ledger — `scan_audit_file` restores an entry for every Decision event in durable history — so a client told `broker-unavailable` would loop against a permanently capped broker. The server logs `broker_capacity_exhausted` alongside the refusal; the fix is an operator raising `brokerLimits.maxReplayIds` or `serverLimits.auditMaxRecords`, or moving the audit file aside, not anything the caller can do.

Size `maxReplayIds` against `auditMaxRecords` rather than below it. The ledger bound is cumulative across restarts for the same reason: one restored identifier per durable Decision event. A denial costs one audit record and one ledger slot, while an executed invocation costs two audit records and one slot, so a denial-heavy history exhausts a ledger sized at half the audit budget *first* — before the designed `AuditError::Full` refusal the audit bound exists to produce. The stock `maxReplayIds` of 100 000 is therefore undersized for the chart's `auditMaxRecords: 200000`, which is why the chart's default `broker.yaml` sets `brokerLimits.maxReplayIds: 200000` outright. `brokerLimits` defaults field by field, so raising one bound does not mean restating the other; raise the two together.
A DRN refusal is a normal denied invocation with reason `secret-denied`; unknown, unbound,
wrong-sink/username and Cedar-denied names deliberately share it. A source that is missing,
malformed, oversized, unavailable or times out after authorization produces a normal failed
invocation such as `secret-resolution`, plus a terminal execution audit record with zero HTTP
calls. No provider work began, so retry is safe but useful only after the source condition changes.

A failure response is not the only way to reach that state. Nothing ties a client's `io_timeout` to broker-side execution deadlines, so a client whose response read fails is in the same position: the complete request frame was delivered and the outcome is unknown to it. `ClientError` therefore records which half of the exchange failed — a request-phase framing failure delivered nothing, a response-phase one delivered everything — and `ClientError::may_have_executed` covers both that case and the `outcome-unaudited` code. A caller submitting a write must map it to a non-retryable result: `dekopon-agent` reports it to a script as `denied` (exit `126`) rather than as a generic failure, because a retry carries a fresh invocation identifier and replay rejection cannot recognize it as a duplicate.

Invalid informational reports are also diagnosable server-side without widening the wire: `AgentInventory::validate` and `ModelUsageReport::validate` name the offending agent and the exact bound, `dekopon-brokerd` logs that as `broker_agent_inventory_rejected` / `broker_model_usage_rejected`, and the response stays the generic `invalid-request`.

Successful informational reports return `acknowledged`; invalid bounds return `invalid-request`, and a mapped peer without an attestor grant receives `unauthenticated`. Reporting failures are non-authoritative and must never be interpreted as provider work or retried as an invocation.

## HTTP component contract

`dekopon:http@1.0.0` is a high-level request/response interface rather than a socket or generic I/O interface. Its request shape carries:

- an arbitrary valid HTTP method token, including standard and extension methods;
- an absolute URI;
- ordered headers with duplicate values preserved;
- a buffered byte body.

Its response carries a status code, ordered headers, and a buffered byte body. Failures use a bounded typed error rather than traps or transport-library error text.

The first version intentionally has no streams, polling, sockets, DNS, filesystem, environment, or raw credential imports. The broker may use a streaming native HTTP client internally, but it presents bounded buffers at the component boundary. If measured workloads later require guest-visible streaming, a separate interface can use standard `wasi:io` resources without granting raw sockets.

The interface can represent every HTTP method, but representation is not permission. Each invocation's policy constraints determine the allowed methods and destinations.

## Broker HTTP enforcement

Before performing a request, the broker host validates all of the following against trusted authorization state:

- the invocation is authorized for the currently executing capability;
- the HTTP host and effective port are explicitly allowed;
- the method is explicitly allowed;
- the scheme is HTTPS, except for explicitly enabled loopback test endpoints;
- user information and URI fragments are absent;
- header names and values are syntactically valid and within count and byte limits;
- authority-defining, hop-by-hop, proxy, and broker-managed credential headers are not guest controlled;
- the request body and complete encoded request remain within their limits;
- the invocation has remaining host-call budget.

The native client does not inherit proxy configuration from the environment, does not follow redirects, and disables automatic decompression. DNS results are checked against destination rules and pinned into the client before connection, so a later resolver answer or redirect cannot escape that decision. Response headers and bodies, host calls, Wasm memory and fuel, serialized input/output, and wall-clock duration are bounded. Timing out an invocation drops the async Wasmtime/HTTP operation and releases its fresh store.

Provider credentials remain broker-owned. Two selection paths are current: legacy implicit credentials, and separately authorized public DRNs. An owner-only credentials file (strict `dekopon.dev/broker-credentials/v1alpha1`, mode `0600`, single-link, byte-capped) resolves symbolic names into destination-bound `authorization` values held in `Redacted` wrappers. A constraint set binds a default name with `credential:` and optional per-agent overrides with `credentialByAgent:`; construction fails closed for *every* name the set can select unless it exists, the set grants HTTP authority, and every `allowedHosts` entry appears verbatim in that credential's `destinations` — which makes a runtime destination mismatch unreachable. The native engine injects the header strictly after guest-header validation (a guest-supplied `authorization` is still rejected, never overwritten), only for requests whose resolved authority falls inside the binding, refusing rather than sending unauthenticated otherwise. The injected header counts against neither guest byte grants nor public evidence sizes; evidence and audit record only `credentialInjected: true`. Raw credentials are never returned to the provider, model, client, trace, evidence, or normal audit fields, and destination binding plus redaction are independently tested at the engine, broker, and service layers. A DRN-backed call additionally commits the exact sink/use binding into authorization; the host refuses a swapped credential and direct raw/rendered reflection.

### Selecting a credential per agent

One capability presents one credential to everyone by default. `credentialByAgent` adds the second axis: the same operation may present a different broker-held secret depending on which agent is acting, so two organizations can be reached by two tokens through one capability rather than through a duplicated provider namespace.

Selection keys on the agent because that is the identity the deployment already partitions on — a route binds a transport and a match to an agent, so per-agent credentials are per-workspace and per-channel scoping for free. It is trusted input rather than a caller claim: the agent name arrives in the `AuthenticatedContext` the broker itself derived from an owner-configured attestor grant and identity mapping, never from an invocation payload, and the map that interprets it is owner-authored configuration in the same file as the rest of the execution bounds. A caller carrying no agent at all — a direct `dekopon-run` peer with `Actor::Service` — matches no override and takes the default. A set with no default and no matching override keeps the original meaning of an absent credential: the capability transacts unauthenticated.

Cedar still cannot reach any of the legacy selection map. A policy edit can broaden who may drive an agent; it can never bind a legacy credential, and it cannot make an agent present a token the constraint set did not already give it.

### Model-selected public DRNs

A DRN is the deliberately different path. The model selects an inert logical name in a typed
top-level proposal, never provider JSON. The broker first permits the capability, then asks Cedar a
second question: may this authenticated principal use exactly this `Dekopon::Secret` for the named
capability/provider/native sink? An owner-only `SecretUseBinding` must independently match and is
narrower than the capability's HTTP constraints. The resulting `SecretUseGrant` binds DRN, sink,
binding ID, authority, method, canonical exact/segment-prefix path, query policy and injection count
into the authorization serialized for evidence.

Only after durable decision audit does `dekopon-brokerd` resolve one invocation-pinned snapshot from
the private source. `dekopon-broker-host` equality-checks the resolved credential against the grant,
and `dekopon-http-host` renders strict Bearer or Basic at the final boundary. No provider or HTTP WIT
change is involved. Existing implicit credentials remain the right choice when a model has no reason
to select among secrets. [`secrets.md`](secrets.md) defines DRN grammar, map schema, all current
adapters, path matching, rotation and reflection limitations.

## Authorization policy

Broker policy is [Cedar](https://cedarpolicy.com), and it is deny-by-default at every layer. It
answers exactly one question — may this principal take this action on this resource in this
context — and it answers nothing about how the resulting invocation executes.

### Two files, two jobs

Execution constraints live in owner-authored **constraint sets**, one per capability, outside the
policy language entirely. A constraint set names the provider route, the trusted
effect/risk/idempotency classification, an optional symbolic credential plus optional per-agent
overrides of it, and the bounded execution authority: timeout, output ceiling, exact destination
hosts, allowed methods, host-call count, and byte ceilings. Construction validates all of it
against the loaded provider manifest, the component host's independent ceilings, and the credential
store.

The split is the point. A policy edit can broaden *who may act*; it can never widen a timeout,
reach a new host, or bind a credential that was not already bound. And a capability with no
constraint set is simply not deployable — the broker denies it `unconstrained-capability` before
consulting policy, and refuses to start if any policy could ever permit it.

### The entity model

Everything is in the `Dekopon` namespace: `Dekopon::Principal`, `Dekopon::Provider`,
`Dekopon::Agent`, `Dekopon::Secret`, one `Dekopon::Action::"<capability-id>"` per loaded capability,
and the fixed `agent.prompt` and `secret.use` actions. Capability actions apply to a `Principal`
over a `Provider`; `agent.prompt` applies over an `Agent`; `secret.use` applies over one exact DRN.
No entity carries attributes.

Capability actions carry a context of `{ via?, subject?, agent?, effect, risk, idempotency }`;
`agent.prompt` carries `{ via?, subject?, agent? }`. Those values come from authenticated transport
state or owner-controlled configuration. Message content and arbitrary provider input remain absent.
`secret.use` is the one settled caller-supplied exception: its resource is a strongly validated
public DRN and its fixed context names capability, provider and sink; it still grants nothing without
the owner-authored execution binding.

`agent.prompt` is the session gate. Permitting a principal to drive an agent is now its own
explicit statement, checked before an attested `capabilities` answers and before an attested
`invoke` authorizes anything; a denial is the audited reason `agent-denied` under the attested
context.

### Startup validation

The schema is generated from the deployment's declared world — the principals its peer identities
and subject mappings name, and the providers and capabilities its loaded manifests expose — and the
policy set is validated against it in Cedar's strict mode. An unknown action, unknown entity type,
or ill-typed expression refuses startup.

Cedar's validator checks types, not instances, so the engine separately classifies every *principal*,
*provider*, and *action* literal against that world: `principal == Dekopon::Principal::"typo"` is
well typed and would simply never match, and catching it at startup is what the exact engine's
reachability check used to buy. An undeclared principal is always fatal; an undeclared provider or
capability is fatal only under `strict: true`, and is otherwise reported and registered as a
schema-only phantom, because it names a provider the deployment has not dropped in yet rather than a
typo. Templates are refused, source is capped at 1 MiB and 1024 policies, and empty policy text is
valid and permits nothing.

**Agents are the deliberate exception, and it is the one place a typo survives startup.** The agent
catalog belongs to `dekopond`, not to the broker, so the broker declares the `Dekopon::Agent` type
and matches instances by UID without enumerating them. `resource == Dekopon::Agent::"revewer"`
therefore validates, starts cleanly, and then matches nothing — every session that agent should have
been permitted is denied `agent-denied` at runtime, which is exactly the latent-dead-policy failure
mode the entity-literal check exists to prevent, in the one place the check does not apply. The
gateway's own configuration is the check that remains: a route naming an agent the catalog does not
contain is a `dekopond` startup failure. Cross-read the two files when a grant is written, and see
the Cedar name table in
[`crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md#policy).

### Decisions

A component's import table and manifest can narrow what it is able to request, but neither can
widen policy. The broker rejects unknown imports before invocation. A provider that declares a
read-only capability cannot use a method authorized only for an external-write capability, and one
capability's grant is not reusable by another invocation.

Every decision produces a stable decision identifier, the policy revision, the identifiers of the
policies that determined it (`policy_ids`), and a digest of the evaluated policy set
(`policy_digest`). Authorization binds that decision, the original proposal, the selected provider
and capability, and the exact execution constraints used by the host. Any Cedar evaluation error
denies, and surfaces as a stable flag rather than error text — a denial explanation must not become
a per-request channel for policy source.

## Evidence and audit

The current core appends one decision event for every decoded invocation from a mapped peer and a terminal execution event after each completed authorized attempt. Events correlate authenticated principal, actor, authorizing broker, invocation and trace identifiers, capability, provider, policy decision, determining policy identifiers, policy-set digest, effect/risk/idempotency classification, the symbolic name of the selected credential, timing, outcome, output digest, and bounded HTTP metadata. Public results carry the same decision linkage and digest evidence. Request input, provider output, URL paths/queries, headers, bodies, cookies, credentials, and model text are not written to audit fields.

A legacy credential's *name* is deliberately in that list and its value is deliberately not. A DRN-backed decision/execution instead carries optional public `secret` and `secret_sink` fields; the legacy `credential` field stays absent. The name is owner-authored configuration that already sits in `broker.yaml`; once one capability can present two credentials, a record omitting it makes two external writes to two different organizations indistinguishable, and "which authority did this write use" is exactly what an auditor is reconstructing. `credentialInjected` in the per-call HTTP evidence still reports whether a given call presented one. The field is absent when an invocation selected no credential, so a record written before per-agent selection existed decodes and re-serializes to the same bytes it hashed over.

The bounded in-memory implementation hash-links events for tests. `FileAuditLog` persists exclusively writer-locked owner-only bounded JSONL, verifies the complete existing chain before append, synchronizes every decision/outcome, rejects partial writes, reconstructs replay IDs on restart, and can verify an exact count/head prefix. `dekopon-brokerd` maintains that count/head in a separate strict owner-only checkpoint under its own writer lock. It writes audit first, then synchronizes and atomically replaces the checkpoint; startup fails if a non-empty audit has no checkpoint or the checkpoint is not an exact verified prefix. A valid checkpoint exactly one record behind the audit is the recoverable crash window and is advanced before listening; a larger gap fails closed.

This detects valid-prefix truncation relative to the retained checkpoint and makes the head available to an external verifier. It is local integrity evidence, not tamper-proof storage against a compromised broker host: coordinated rollback or deletion of both files requires independent checkpoint retention to detect. Durable remote anchoring, key-backed signatures, tenancy, and incident-response machinery remain separate work.

## Storage is a sibling privileged host interface

**Status: current.** `dekopon:storage@0.1.0` is independent of HTTP.
Constraint sets select exactly `jsonl` or `durable-files`, read-only or read-write, and chat
namespace; combining HTTP and storage is refused. The broker derives every opaque namespace from
the authorized context, consumes a host-instance/invocation/capability/provider-bound grant, and
commits only a valid `Succeeded` response. Stable public classes are `storage-quota`,
`storage-busy`, `storage-timeout`, `storage-corrupt`, `storage-io`, and
`outcome-unaudited`. A chat-memory-routed capability additionally allowlists only
`memory-corrupt`, `result-too-large`, `dedup-conflict`, and `dedup-capacity`; arbitrary provider
messages remain `provider-failure`.

Chat scope is not inferred from subject authority. The claim includes configured transport ID,
transport kind, canonical channel, and canonical conversation; owner configuration grants explicit
breadth and Cedar sees those four trusted optional context fields. Each swapped/malformed/overbound
field denies before namespace creation.

The gateway receipt proves complete transport acceptance (service acceptance for Slack, Telegram,
and Discord; kernel acceptance for local), not human receipt. One dedicated record request follows,
with no automatic retry after response loss or outcome-unknown.

## Version and implementation policy

WIT package versions and Rust crate versions are independent:

- provider crates depend on the WIT interface version they import;
- one broker host crate may register adapters for multiple supported WIT versions;
- compatible native HTTP-library upgrades do not require provider rebuilds;
- breaking component-contract changes receive a new WIT interface version;
- published WIT versions are immutable and resolved through the checked registry configuration and lock file.

The broker fails closed when no approved implementation exists for an import. Runtime provider requests never trigger package or native-code downloads. Optional executable adapters, if introduced later, must be operator-installed, digest-pinned Wasm components rather than native Rust plugins.

## Delivery sequence

The behavior lands in reviewable slices without temporarily granting authority to the immediate host:

1. **Implemented:** publish and validate the HTTP WIT contract and guest bindings.
2. **Implemented:** generalize provider guest world generation and add an HTTP-importing fixture that the immediate host still rejects.
3. **Implemented foundation:** add the bounded native engine and asynchronous broker-owned component host with loopback mock-server tests.
4. **Implemented foundation:** add deny-by-default policy, trusted context binding, single-use authorization, bounded replay state, digest evidence, and an in-memory verifiable audit chain.
5. **Implemented foundation:** add owner-only durable audit persistence, restart verification, checkpoints, and replay-ID restoration.
6. **Implemented client foundation:** add strict bounded local framing and an unprivileged Unix client with no identity or authorization payload fields.
7. **Implemented service foundation:** add the authenticated owner-only Unix listener, exact peer-UID context mapping, secure socket lifecycle, bounded concurrency/draining, and broker executable.
8. **Implemented demonstration:** add mock-backed JSONPlaceholder post-read and external-write capabilities with exact method/authority policy and audit-redaction tests.
9. **Implemented client integration:** add explicit `dekopon-run broker capabilities/invoke` commands that validate server UID and submit identity-free proposals without changing the direct linker.
10. **Implemented durability foundation:** add a separately locked atomic checkpoint file, startup prefix verification, rollback rejection, and audit-ahead crash recovery.
11. **Implemented credentials:** broker-owned destination-bound credential resolution with per-capability symbolic binding, construction-time coverage validation, post-validation header injection, and independently tested destination binding and redaction.
12. **Implemented policy engine:** replace exact matching with Cedar over a generated, strictly validated schema; keep execution constraints in owner-authored constraint sets; add the `agent.prompt` session gate and determining-policy explanations in audit.
13. **Implemented per-agent credentials:** let one constraint set name a default credential plus per-agent overrides, validate every selectable credential's existence and destination coverage at startup, and record the selected symbolic name in the terminal audit event.
14. Add independently retained, signed, or remote checkpoints before production claims.

Until a remaining slice is implemented, its behavior remains committed direction rather than current functionality.
