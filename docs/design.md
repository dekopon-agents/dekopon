# Dekopon design

This document is the design entry point for both human contributors and coding agents. It records the product model, accepted invariants, component responsibilities, and the boundary between what exists and what is planned.

Use these status terms consistently:

- **Current** — implemented and testable in this repository.
- **Committed direction** — an accepted constraint for future work, but not necessarily implemented.
- **Exploration** — an option that still needs a decision; it must not be presented as a feature or invariant.

The roadmap controls sequencing, not design authority. Code and tests demonstrate current behavior; this document and the security model state the constraints that new behavior must preserve.

## Constitution

Dekopon is an extensible runtime for self-hosted AI agents. Providers are WebAssembly
components; the model proposes, a separate broker authorizes and executes; and provider
credentials can never reach the model.

Three goals, in priority order. A change is judged by which of these it serves. A subsystem
that serves none of them is a deletion candidate, however well built.

1. **Credentials are unleakable.** A model may reference a secret; it cannot read any. Secret
   bytes exist only inside the broker: never in the model, the shell, the gateway, the wire
   protocol, provider memory, evidence, audit, or telemetry. The native HTTP engine injects
   them after the component has built its request. The authorized endpoint necessarily
   receives the credential and is trusted by construction.
2. **One trace, complete.** Dekopon is the operator's agent system, not the end user's
   private system. An operator with access to the telemetry store can reconstruct everything
   that happened when an agent ran: every inbound message and its sender, every prompt and
   model answer, every script, every command word with its arguments and output, every
   proposal with its arguments, every decision, every provider input and output, every HTTP
   egress. All of it rides one W3C trace per message, from transport receipt through
   `gateway.session`, model turn, `shell.command`, `broker.invocation`, `provider.invoke`, and
   native HTTP egress, as span attributes or as log records sharing the trace id. The broker
   parents its spans on the request's `traceParent`. The W3C trace id is the only correlation
   identifier and every audit record carries it. The only exclusions are secret bytes and the
   gateway's own credentials (chat tokens, model keys, OTLP headers); that is goal 1's job, not
   telemetry's. Completeness beats volume: an attribute may be truncated with a marker, a span
   is never dropped. No `traceparent` header is sent to a third-party endpoint. The telemetry
   store is inside the operator's trust boundary. The `telemetryPayloads` gate, every metadata-only
   mode, the `<withheld>` command word, and the 256-span INFO cap are gone: a command word is
   recorded and no span is dropped. *Committed direction:* the argument-count-only attribute
   gives way to the arguments themselves.
3. **Extensible through Wasm providers.** New capability arrives as an out-of-tree component
   with a manifest and command words, executed under exact owner-authored constraints. Nothing
   in this tree grows to add a capability.

### Non-goals

Decided 2026-09-06 and 2026-09-10. An argument for keeping or adding code on any of these
grounds is rejected on sight.

- **Idempotency, exactly-once, duplicate-effect defense, automatic retries.** If a call fails
  the model re-assesses and retries. *Committed direction:* the `idempotency` capability field
  is removed.
- **Crash durability, reconcilable state, audit tamper-detection.** Audit is one structured log
  record per broker decision, `broker.decision` or `broker.execution`, emitted inside the trace:
  stdout JSON always, OTLP logs when `telemetry` is configured. There is no on-disk audit sink;
  losing the log exporter loses audit.
- **Transformed reflection at the endpoint.** The host refuses a response that carries the raw
  secret; an endpoint that re-encodes what it legitimately received is outside the boundary.
- **End-user privacy from the operator.** Telemetry is not minimized toward the operator; a
  metadata-only telemetry mode does not exist. A person talking to a Dekopon agent is talking
  to its operator.
- **A compromised host, root, or a malicious process in the broker's trust domain.**
- **Production-sandbox claims for Wasmtime.**

### Invariants

1. **A proposal is not authority.** Anything may create a `ProposedInvocation`; only the broker
   may create an `AuthorizedInvocation`.
2. **Capabilities are explicit and narrow.** Read authority never implies write authority.
3. **Identity comes from authenticated transport**, never from model, repository, or payload
   content.
4. **Provider credentials belong to the broker boundary** (goal 1).
5. **Authorization is bound to execution.** A grant carries the proposal, the decision, and the
   constraints the host enforces.
6. **Every run is reconstructible from the telemetry store.** Decision, outcome, audit record,
   prompts, and arguments share the W3C trace id (goal 2).
7. **External-write authority requires process isolation.** `dekopond` and `dekopon-brokerd`
   stay separate processes with separate UIDs.
8. **Documentation distinguishes reality from direction.** Current, Committed direction,
   Exploration.

## System model

### Core concepts

- A **principal** is an authenticated human or service identity.
- An **actor** is the identity attributed to an operation by trusted infrastructure; it may represent a human, service, or agent.
- An **agent** is an orchestration configuration. Its capability list defines what it may propose, not what its process may execute directly.
- A **capability** names a narrow operation, its provider, effect kind and least-privilege provider permissions.
- A **provider** is a declaration for an integration boundary. Existing credentials are selected symbolically by trusted configuration; a model-selected public DRN is separately typed, authorized, and matched to an owner-only use binding before the broker resolves it.
- A **proposal** is untrusted intent plus arguments.
- An **authorization** is a broker-owned state transition that binds a proposal to constraints and a decision receipt.
- **Evidence** supports later verification of a decision or execution result. *Committed direction:* evidence digests and decision receipts are removed; the audit record inside the trace carries the decision.
- An **audit record** links trusted identity, proposal, decision, effect, outcome, and evidence.

A resource declaration is not a live connection and a status authored in local configuration is not a cryptographic attestation.

### Authority flow

```text
untrusted model/repository content
              |
              v
     ProposedInvocation
              |
              | authenticated transport + trusted mapping + policy input
              v
       authorization broker
          /           \
       deny         authorize
        |               |
        |               v
        |      AuthorizedInvocation
        |        + receipt
        |        + constraints
        |               |
        |               v
        |       capability provider
        |               |
        +-------> InvocationResult
                        |
                        v
                evidence + audit
```

The broker owns the only authority transition in this flow. The authenticated request into the local broker carries a proposal only; trusted context comes from Unix peer credentials and owner-controlled mapping, not an authorized bearer grant or payload fields. `AuthorizedInvocation` is created and consumed inside the broker-owned execution boundary; `dekopond` never receives it or presents its serialized representation as authority. Rust visibility and the absence of deserialization are useful defense in depth, but the actual control comes from authentication, process isolation, policy, credential separation, binding authorization to execution, and enforcement at the provider host.

## Component boundaries

| Component | Authority and responsibility | Status |
|---|---|---|
| `dekopon-core` | Validated identifiers and dependency-light domain types, including the `SkillId` name grammar | **Current** |
| `dekopon-protocol` | Versioned, transport-independent resource shapes | **Current** |
| `dekopon-config` | Config discovery, decoding, duplicate detection, reference validation, and bounded in-memory `Skill` loading from `SKILL.md` directories | **Current** |
| `dekopon-capability` | Capability metadata and proposal/authorization invocation states | **Current**, consumed by broker libraries and service |
| `dekopon-provider-sdk` | Rust guest trait, provider manifests/responses, and default or caller-generated WIT world export adapters | **Current**, experimental component contract |
| `dekopon-provider-http` | Rust guest facade for the buffered `dekopon:http@1.0.0` import; contains no transport or authority | **Current**, bindings only |
| `dekopon-provider-storage` | Feature-gated JSONL and durable-files guest bindings; contains no path, namespace, transaction, SQL, or authority API | **Current**, bindings only |
| `dekopon-http-host` | Statically linked native buffered HTTP engine consuming exact grants beneath independent ceilings; contains no WIT or Wasmtime integration | **Current** library |
| `dekopon-storage-host` | Wasmtime-independent opaque namespace derivation, key/root hygiene, logical quotas, leases, direct invocation handles, JSONL, and durable-files imports | **Current** privileged library |
| `dekopon-broker-host` | Privileged async Wasmtime adapter consuming authorized invocations and exact optional storage grants, linking only versioned Dekopon HTTP/storage imports, and emitting bounded metadata | **Current** library used by the separate broker process |
| `dekopon-broker` | Trusted context binding, Cedar-decided authorization over owner-authored execution constraints, provider execution, digest evidence, and metadata-only audit records emitted as log events inside the trace | **Current** library; its audit sinks are a bounded in-memory log for tests and embedding and the keep-nothing `TraceOnlyAuditLog` |
| `dekopon-policy` | Bounded, deterministic Cedar adapter: generated schema, strict startup validation, declared entity world, deny-on-error decisions, determining policy identifiers, policy-set digest | **Current** library consumed only by `dekopon-broker` and `dekopon-brokerd` |
| `dekopon-broker-protocol` | Lightweight strict versioned bounded frames and Unix client with identity/authority-free payloads and server peer-UID verification | **Current** shared broker/gateway API with no privileged host or native-HTTP dependency |
| `dekopon-model` | Bounded chat-model contract, OpenAI-compatible transport, and ChatGPT/Codex subscription auth and Responses client | **Current**, consumed by the gateway, by `dekopon-brokerd` for the one `chatgptSubscription` refresh implementation, and by external clients; it holds no image client, because image generation is a provider effect |
| `dekopon-shell` | Sandboxed bash-flavored interpreter whose command words dispatch to capabilities through one abstract seam, with its own step, recursion, output, deadline, and capability-call bounds | **Current**; it links no Wasmtime, broker, HTTP, or filesystem code |
| `dekopon-process` | Unprivileged one-run/one-node Tokio lifecycle seam whose internal supervisor preserves a typed operation result or Tokio task failure and delivers it to a required abandonment observer if the outer caller is dropped while the runtime remains alive | **Current** library consumed by `dekopon-agent`'s cancellable `broker-command` node, which a gateway session's Stop abandons through the cooperative abort-then-join `CancelHandle`/`CancelSignal` pair; scopes, ports, deadlines, and stage scheduling remain future |
| `dekopon-agent` | The shared agent session layer: the bounded scripting prompt loop, optional bounded meta tools (asset fetch, `inspect_agent_config`, `read_skill`, and `suggest_improvement`) each offered only when the embedder supplies or enables it, the script runtime spending a session-wide capability budget, a broker-leg facade over the protocol client whose command-word runs are cancellable process nodes and which strips a capability result's reserved `attachments` key into a byte-free reply slot and expands a `chat-asset:<N>` input marker for the capabilities an embedder lists | **Current**, holding no authority; it depends on `dekopon-config` for the `Skill` type, on `dekopon-process` for that node, and on no broker crate beyond the protocol client; consumed by `dekopond` in this tree, and by out-of-tree clients such as `dekopon-console` |
| `dekopon-telemetry` | OTLP exporter settings and subscriber wiring, with ingest credentials read only from the environment | **Current** library shared by the exporting executables |
| `dekopond` | Chat-transport wakeups, including a signed text-only WhatsApp Cloud API webhook, attested routing, opt-in route-scoped delivery of provider-produced attachments and chat-asset capability inputs, text/attachment replies, authorization-fed Slack Agent thread ownership, optional no-reply decisions, best-effort native in-flight activity, cooperatively cancellable bounded agent sessions with no broker authority, credential-free self-inspection, and bounded scope-aware conversation history | **Current** unprivileged daemon; a route is one independent session per message unless it opts into `mode: persistent` (private per subject by default, explicitly shareable only inside one agent/transport/conversation), attachment delivery, chat-asset inputs, and activity are explicit opt-ins after authorization, Slack continuation is installed only after authorization, WhatsApp TLS terminates outside the daemon and replay handling is process-local, and the chart enforces the [current local process boundary](security-model.md#current-local-process-boundary) |
| `dekopon-brokerd` | Unix peer-UID authentication, Cedar authorization, destination-bound `credential`/`credentialByAgent` bindings ([to be replaced by DRNs](#legacy-credential-bindings)), including `chatgptSubscription` refreshed through the shared `dekopon-model` implementation, public-DRN/private-map secret resolution after a separate `secret.use` decision, provider execution, evidence, audit as log records in the trace, and an operator command tree that runs without serving the broker: the exact-reference OCI provider manager (`provider sync|list|verify`, of which only `sync` reaches the network) | **Current** privileged process; secret descriptors are parsed without network at startup and one source snapshot resolves per authorized invocation; managed-provider startup remains network-free; workload-identity secret bootstraps, leased secrets, provenance verification, SemVer updates, and pruning remain future |
| Deployable privileged provider path | Authenticated broker ownership of policy, credentials, component-host execution, digest evidence, and authorized effects | **Current** local Unix foundation; stronger deployment transport remains direction |

The agent daemon must not gain effect authority merely because it coordinates a task. The broker must not perform model orchestration merely because it can execute a provider. Image generation is a provider effect like any other external write: the credential lives behind the broker, Cedar decides each call, and the broker audits it. The gateway holds no image credential. On an opted-in route it delivers bounded PNG attachments from an authorized capability result to the authenticated reply target and expands `chat-asset:<N>` input markers for listed capabilities. It is a courier, not an authority; attachment bytes bypass the model.

### Provider storage and durable chat memory

**Status: current.** `dekopon-brokerd` may opt into a separate broker-only
storage root. Exact `jsonl` or `durable-files` plus read-only/read-write authority
is bound to one authorization; HTTP and storage cannot coexist in one v1 capability. Raw scope and
logical names never select paths. Each host call applies its mutation directly. A failed provider result or trap does not undo
completed writes ([non-goals](#non-goals)).

The independently released optional `memory-chat` provider uses JSONL only. Which capabilities make up
the surface is the owner's declaration — one `route:` per record/recent/search role in
`constraintSets` — not a reserved name, so renaming the provider drops no reservation and naming an
ordinary capability `memory.chat.export` gains none. Hidden recording is reachable solely through
`recordDeliveredTurn` carrying a chat attestation, after complete gateway-attested transport
acceptance. Recent and literal case-insensitive search are on demand and never automatically seed a
prompt. Both
continuity policies always include provider, agent, canonical sender, transport, channel, and
conversation: `stable` survives semantic authority changes; the default
`authority-bound` persists an opaque pointer and random epoch so A→B→A creates three generations.
The store has finite permanent deduplication and no deletion/export UX or encryption-at-rest claim.

Slack Agent channel continuation is also current. One explicitly
addressed, freshly authorized message claims an exact workspace/channel/thread/sender tuple in a
bounded gateway-only registry. Only that sender's later message in that thread bypasses the repeat
mention, and every continuation is authorized again. The prompt marks that unaddressed follow-up as
optional and offers one payload-free decline tool; choosing it before capability work produces no
chat post, acceptance receipt, or durable-memory record. Capability work makes a reply mandatory;
if no model turn remains, the gateway posts a fixed warning to inspect audit before retrying.

## Current control paths

The gateway loads a typed catalog through `dekopon-config`, which rejects unknown fields,
invalid identifiers, duplicates and cross-resource reference problems in one refusal.

Model-account lifecycle is a separate operator path that does not resolve or parse the catalog:

```text
parse dekopond auth chatgpt CLI
  -> contact OpenAI's fixed device-auth endpoint
  -> store, inspect, or remove Dekopon's isolated credential file
```

The current invocation path is the gateway's bounded `dekopon-agent` prompt loop and
sandboxed `dekopon-shell` interpreter, submitting proposals through the unprivileged
broker protocol client. Only the separate broker authorizes and executes provider effects.
Mounted skills and opt-in improvement suggestions are shared agent tools, not capabilities.

## Resource and API design

Authored resources use a compact Kubernetes-inspired shape:

```yaml
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata:
  name: reviewer
spec:
  description: Reviews pull requests
  capabilities:
    - github.pull-request.read
status: Ready
```

Design rules:

- API versions and kinds are explicit on the wire.
- Metadata names are validated as kind-specific identifiers before entering the catalog.
- Unknown authored fields are rejected. Silently ignored authority settings are more dangerous than an early compatibility break.
- Lists and rendered resources are deterministically ordered.
- Protocol types do not depend on a transport.
- Configuration is parsed once into protocol/domain resources.
- Validation scans the whole catalog and reports every problem at once. Stopping at the first conflict makes an operator rediscover the next one after every fix.
- A future network API must document negotiation and field-preservation rules before relaxing strict decoding.
- Alpha resources may evolve, but changes must update examples, round-trip tests, schemas, and operator documentation together.

## Invocation lifecycle

The state model is asymmetric:

```text
Proposed --broker denies----------------------> Denied result + evidence
    |
    +--broker authorizes--> Authorized --host executes--> Succeeded/Failed result
```

`ProposedInvocation` is publicly constructible because untrusted callers are allowed to express intent. `AuthorizedInvocation` is not publicly constructible from arbitrary fields. Broker authorization must validate its decision metadata and attach bounded `ExecutionConstraints`. Those constraints never come from the policy set: Cedar decides only whether the invocation is permitted, and the bounds come from the capability's owner-authored constraint set, so no policy edit can widen an execution bound.


The current local broker protocol and service define:

- peer-UID-authenticated principal and actor mapping with no payload identity fields;
- attested on-behalf-of proposals: a peer holding an owner-configured attestor grant may name a canonical external subject, and the broker alone maps that subject to a principal through owner-controlled configuration;
- `via` as policy context, so a policy that requires an attestor cannot authorize a direct peer and one that forbids it cannot authorize an attested proposal — configuring a gateway cannot widen a grant that already exists;
- an explicit `agent.prompt` action, so permitting a principal to drive an agent's session is its own policy statement rather than a side effect of holding any capability;
- canonical proposal and decision identifiers;
- policy revision, the determining policy identifiers, and a policy-set digest in every decision record;
- timeout, output, exact network, and host-call constraints;
- evidence digests and one audit log record per decision;
- denial and partial-failure semantics.

## Deployment and provider isolation

For current and future external writes, the deployment boundary is:

```text
dekopond              unprivileged orchestration and model interaction
      |
      | authenticated proposal connection
      v
dekopon-brokerd       policy, credentials, provider execution, effects
      |
      | broker-owned constrained host call
      v
Wasm provider          one narrow integration operation
```

Both processes deploy separately over the local Unix transport; the WhatsApp public webhook terminates only in the unprivileged gateway and preserves the same separation. `dekopond` sends proposals on that authenticated connection and receives results; it does not receive or relay an `AuthorizedInvocation` as a wire grant. Its proposals are attested: it names the chat sender's canonical subject and the agent answering, and the broker alone maps that subject to a principal. Its contract is documented in [`dekopond.md`](dekopond.md). The automatic scope-aware replay window lives inside that unprivileged process and never reaches the broker as authorization input; it is private per subject by default and explicitly shareable only inside one agent/transport/conversation. Optional durable turns travel only through the hidden storage-provider path, and neither caches authorization: a persistent conversation opens a fresh attested leg per message exactly as an independent session does. The broker may share a Wasmtime engine and compiled component cache, but each invocation gets a fresh store. Privileged providers run as bounded asynchronous invocations integrated with Tokio, with explicit limits on time, memory, output, network destinations, and host calls.

The JSONPlaceholder example proves separately named read-only and external-write provider operations against loopback mocks; its optional mock endpoint cannot widen exact broker authority. The privileged `dekopon-broker-host` uses Tokio, exposes only statically implemented Dekopon HTTP and storage interfaces, consumes constrained authorization plus an exact single-use storage grant where applicable, and creates a fresh bounded store per operation. `dekopon-broker` binds a separately supplied authenticated context, asks `dekopon-policy` whether that context may act, binds an allow to the capability's owner-authored constraint set, constructs and consumes authorization, and emits redacted decision/outcome metadata as audit log records inside the trace; where those records go is the [`dekopon-brokerd` audit contract](../crates/dekopon-brokerd/README.md#audit). The protocol/client library adds hard-bounded strict frames and verifies a configured server UID. `dekopon-brokerd` maps connected peer UID into trusted context, binds a protected Unix socket, limits and drains connections, and exposes policy-authorized provider execution. Its separate `provider` operator mode resolves fully qualified exact OCI tags or manifest digests into a strict generated lock and immutable local blobs, while ordinary startup performs no network access; a managed startup derives blob paths from that lock and compares expected length, component SHA-256, and provider ID with the exact buffer and bounded description the host consumes. Directly named `providers` stay supported and acquire no remote provenance. It also derives attested contexts: a peer's owner-configured attestor grant bounds which canonical subject namespaces it may speak for, owner-configured identity mappings alone turn a subject into a principal, and policy conditioned on `context.via` decides what that attested context may do. The peer supplies the subject and never the principal, and every refusal is an audited denial recorded against the peer. The chart isolates gateway and broker UIDs under the [current local process boundary](security-model.md#current-local-process-boundary). The broker resolves destination-bound `credential`/`credentialByAgent` values from a separate owner-only file, selecting per acting agent where a constraint set names one. These legacy bindings [will be replaced by public DRNs](#legacy-credential-bindings); the current ChatGPT kind renews through the shared `CredentialFile` before provider execution. Separately, an inert public DRN may arrive as typed proposal data: the broker requires ordinary capability policy, an exact `secret.use` Cedar grant, and an owner-only use binding before resolving one invocation-pinned source snapshot and handing only native Basic/Bearer material to the HTTP engine. The provider execution boundaries are defined in [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries). Cedar sees principal, action, provider, `via`, subject, agent, optional trusted chat transport/channel/conversation scope, and the trusted classification; arbitrary provider input is absent from its context. The sole typed caller-supplied exception is a public DRN in a separate `secret.use` request whose schema fixes the capability, provider and sink; possession grants nothing and the owner binding remains the execution ceiling.

## The granularity of authority

**Status: current.** Five separate mechanisms decide what one message may cause, and each is the
unit of a different thing. This section states them as one model,
because the deployment shape they compose into is not visible from any of them alone.

- **Agents are the unit of surface.** A catalog agent has its own capability list and its own
  instructions. Two agents can name overlapping capabilities and remain separate surfaces.
- **Routes are the unit of reach.** A route binds a transport and a match — a direct message, one
  named channel, or any channel the bot is summoned in — to one agent. So a Slack workspace, a
  Discord server/channel, or a single channel inside either selects which agent answers.
- **Principals are the unit of trust.** Each canonical subject maps to its own principal through
  owner-controlled configuration, so one human in two workspaces is two principals with two
  surfaces, revocable independently. The gateway names a subject and never a principal.
- **Policy is the unit of permission.** A Cedar statement grants one principal one action on one
  resource, conditioned on `context.via` and `context.agent`. "This person may drive this agent,
  through this gateway, with these capabilities" is one statement rather than an emergent property
  of several.
- **Constraint sets are the unit of execution.** Timeouts, output ceilings, exact hosts, methods,
  call budgets — and the credential, which may differ per acting agent. None of it is reachable
  from policy text.

What the composition buys: **two organizations, two tokens, two agents, one broker.** A
DekoponVille agent reachable from one channel presents a `dekopon-agents`-scoped token; a Nested Set
agent reachable from another presents a `scientist-hq`-scoped one. No capability is duplicated, no
provider is deployed twice, and no token is reachable from the wrong workspace — because the route
decides the agent, the agent decides the credential, and the policy decides whether that principal
may drive that agent at all. Revoking one is one policy statement or one mapping, and it does not
disturb the other.

What it does not buy: **general capability policy cannot bind a provider-input path.** Cedar
sees the principal, action, provider, and trusted routing metadata; it does not inspect arbitrary
provider JSON, so no capability statement can say "this agent may comment on issues in
`dekopon-agents/*` only". The public-DRN path is narrower: a private secret-use binding
may constrain HTTP authority, method, canonical path and query presence at the native sink, but that
constrains where one secret is presented rather than interpreting repository/object identity in
provider input or request bodies. Upstream credential scope remains the boundary for those semantics.

## Operator interface

The daemon executables own their operator commands: `dekopond auth chatgpt` manages the
isolated model credential before any gateway runtime/configuration, while `dekopon-brokerd`
owns provider lifecycle operations and the bounded authenticated health probe. These are the
only two shipped binaries; neither provides a general runner or catalog CLI.
The interactive [dekopon-console](https://github.com/dekopon-agents/dekopon-console) ships independently.
See [`cli.md`](cli.md) for authentication syntax, formats, guards, and exit codes.

## Accepted implementation decisions

| Decision | Rationale |
|---|---|
| One Cargo monorepo | Initial crates share versions, CI, issues, and security review and are changing together. |
| Cedar for authorization, owner-authored constraint sets for execution | A declarative policy language is the right tool for "who may do what" and the wrong place for a timeout, an allowed host, or a credential binding. Splitting them means a policy edit can broaden who may act and can never widen how far an action reaches. |
| Startup complains, invocation enforces | Whether configuration naming an absent capability refuses startup is an operator preference (`strict`), because that check is a tripwire rather than a control: the `unconstrained-capability` refusal at invocation is unconditional and is what actually denies. Tolerating lets a deployment ship policy that anticipates a provider it has not dropped in yet. A policy naming an absent capability is kept whole and the name registered as a schema-only phantom, never dropped — dropping a grant reading `action in [a, b]` because `b` is unloaded would silently revoke `a` as well, turning one missing provider into a mute agent. An undeclared *principal* is exempt and always fatal: principals come from owner-authored configuration, so naming one that does not exist is a typo rather than an anticipation. |
| Credentials bound per capability, overridable per agent | Which *operation* gets a destination-bound credential is a capability question — the confused deputy is "same component, different operation". Which *credential* an operation presents to a given caller is a separate question, and keying it on the agent reuses the partition routes already make. *Committed direction:* the `credential`/`credentialByAgent` path will be replaced by public DRNs ([migration requirements](#legacy-credential-bindings)). |
| Public DRNs are proposal names, never bearer grants | A model may choose among logical names only as untrusted typed intent. Use requires the capability decision, a separate Cedar `secret.use` decision, an owner binding narrower than capability HTTP authority, and an authorization-bound native sink. The provider sees neither DRN nor value. Implicit credential bindings are supported today but [will be replaced by public DRNs](#legacy-credential-bindings). |
| Edition 2024 with an explicit MSRV | Modern language surface while preserving a tested minimum toolchain. |
| Two daemon executables | Gateway auth and broker provider/probe commands stay with their owning daemon. There is no standalone catalog or invocation CLI. |
| Strong identifier newtypes | Invalid and ambiguous names should fail at system boundaries, not deep in execution. |
| Strict decoding | Misspelled security-relevant fields must not be ignored. |
| `BTreeMap`-backed catalogs | Deterministic reads and output simplify review, testing, and automation. |
| Private authorization fields plus compile-fail tests | Prevent accidental in-process authority fabrication while acknowledging that process isolation remains necessary. |
| Shared SDK host helpers | Manifest validation, conflict reports, store bounds, and engine construction remain available to the broker host, testkit, and external embeddings without granting authorization. |
| Broker-owned buffered HTTP | Privileged providers import a project-owned high-level HTTP contract, while only the separate broker implements networking, applies authorization constraints, and records evidence. |
| No native runtime plugins | Broker host services are statically linked; untrusted imports never trigger Rust library or package downloads. |
| Peer-authenticated Unix broker IPC | Local payloads cannot claim identity; connected peer UID maps exactly to trusted context. The [current local process boundary](security-model.md#current-local-process-boundary) separates gateway and broker UIDs. |
| Desired provider set, generated lock, immutable store | Provider selection, exact OCI resolution, and installed bytes are different states. The lock is the atomic activation point; daemon startup is offline and the host checks the lock against the exact compiled buffer. Exact tags are never implicit SemVer ranges. |
| Gateway-held conversation history | Immediate replay is a compacted, window-bounded in-memory gateway feature: private per authenticated subject by default, or explicitly shared inside one exact agent/transport/conversation with gateway-authored participant attribution. Optional durable memory is a separate broker-owned provider store: content is namespace-bound and model-hidden on write, omitted from audit/telemetry, and retrieved only on demand. Authorization stays uncached in both mechanisms. |
| No empty future crates | A package boundary must be justified by meaningful, tested behavior. |
| Improvement is operator-driven | Skills are catalog resources and `suggest_improvement` notes are telemetry records. There is no durable improvement store, no automatic prompt rewriting, and no grader: an operator reads a suggestion, edits instructions or a skill, and commits the change to the catalog. |

### Legacy credential bindings

**Current:** `credential` and `credentialByAgent` select symbolic entries from the broker's
owner-only credentials file. Entries may be fixed bearer tokens or `chatgptSubscription`
credentials resolved through the shared `dekopon_model::chatgpt::CredentialFile`. Public DRNs
currently use a separate typed proposal, private map, and `secret.use` authorization path.

**Committed direction:** `credential` / `credentialByAgent` bindings will be replaced by public
DRNs. This is a planned migration, not a configuration change implemented by this documentation PR;
existing deployments keep their current bindings until the replacement ships.

The replacement must preserve broker-only secret resolution and injection, per-agent isolation,
exact destination binding, and the ChatGPT credential's one refresh implementation: locking,
adoption, rotation, atomic write-back, failure classification, and the destination-bound
`chatgpt-account-id` companion header. DRNs name the credential; they do not expose its bytes to the
model or provider. Removing the legacy selection path must not remove refresh support or move it
into the gateway or a Wasm component. See [current broker credentials](secrets.md#legacy-credentials-the-broker-renews).

## How to evaluate a proposed change

Before implementation, answer:

1. Which of the three goals does it serve; does it rest on a non-goal?
2. Is the behavior current work, committed direction, or exploration?
3. Which process owns the data and which process owns the authority?
4. Can model or repository content influence a trusted identity or authorization field?
5. Does a read become an implicit write, or a broad capability replace a narrow one?
6. What evidence and audit linkage would the operation need?
7. Does the change preserve typed, transport-independent boundaries?
8. Is a new crate or dependency required by tested behavior today?
9. Which failure, serialization, CLI, and security tests prove the boundary?
10. Which documentation would become inaccurate if the change landed?

If authority ownership is unclear, stop and update the design before adding code.

## Related documents

- [`security-model.md`](security-model.md) — trust assumptions, threat boundaries, and limitations.
- [`architecture.md`](architecture.md) — current crate structure and deployment topology.
- [`development.md`](development.md) — source/test map, generated artifacts, validation, CI, and PR workflow.
- [`cli.md`](cli.md) — current model-auth operator contract, output, and exit codes.
- [`dekopon-agent`](../crates/dekopon-agent/README.md) — shared prompt and session limits.
- [`inference.md`](inference.md) — model request types and wire shape, cache optimization and retention caveats, current conversation memory, and exploratory long-term memory.
- [`dekopond.md`](dekopond.md) — the unprivileged chat gateway's configuration, transports, session bounds, authorization flow, and committed conversation contract.
- [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries) — committed broker-mediated HTTP contract and authority boundary.
- [`secrets.md`](secrets.md) — public DRN proposal, dual `secret.use` authorization, private source adapters, and native Basic/Bearer sinks.
- [`observability.md`](observability.md) — telemetry and audit event names, redaction, and OpenObserve read-back.
- [`roadmap.md`](roadmap.md) — implementation sequence and deferred scope.
- [`README.md`](README.md) — documentation map and task-based reading guide.
