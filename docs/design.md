# Dekopon design

This document is the design entry point for both human contributors and coding agents. It records the product model, accepted invariants, component responsibilities, and the boundary between what exists and what is planned.

Use these status terms consistently:

- **Current** — implemented and testable in this repository.
- **Committed direction** — an accepted constraint for future work, but not necessarily implemented.
- **Exploration** — an option that still needs a decision; it must not be presented as a feature or invariant.

The roadmap controls sequencing, not design authority. Code and tests demonstrate current behavior; this document and the security model state the constraints that new behavior must preserve.

## Product thesis

Dekopon is a capability-oriented control plane for self-hosted AI agents. It should let an operator describe agents and their available capabilities, inspect and control their work, and obtain explicit evidence for authorized effects without granting a model ambient authority.

The operator experience should be understandable in terms of resources and commands. The execution model should be understandable in terms of proposals, authorization decisions, bounded provider invocations, evidence, and audit records.

Success does not mean making a model trustworthy. Success means containing an untrusted model inside a system where authority is narrow, attributable, reviewable, and revocable.

## Non-negotiable invariants

1. **A proposal is not authority.** A model, agent, repository, or tool payload may create a `ProposedInvocation`; only the broker may create an `AuthorizedInvocation`.
2. **Capabilities are explicit and narrow.** Read authority does not imply write authority. Every external write requires a specifically named capability.
3. **Identity comes from authenticated transport.** Model text, repository content, and invocation payloads cannot assert a trusted `Actor`, principal, or workload identity.
4. **Provider credentials belong to the broker boundary.** Agents, prompts, authored resources, normal logs, and evidence records must not contain provider credential values. A public logical DRN may appear as inert proposal metadata, but it grants nothing, resolves only after separate broker authorization, and never enters provider JSON/WIT. Model-endpoint credentials terminate inside the selected model client and never enter provider components.
5. **Authorization is bound to execution.** A grant carries the proposal, policy decision, execution constraints, and receipt needed to prevent a provider host from executing a different or broader operation.
6. **Effects produce evidence and audit linkage.** Proposal, identity, decision, policy revision, execution outcome, and evidence must remain correlatable by invocation and trace identifiers.
7. **External-write authority requires process isolation.** Once writes exist, orchestration and broker authority run in separate processes and deployment units.
8. **Documentation must distinguish reality from direction.** Never describe a daemon, broker, policy engine, privileged provider interface, or external effect as available before it is implemented and tested. The immediate read-only component host must not be presented as the future broker host.

These invariants are more important than API convenience, model autonomy, or architectural symmetry.

## System model

### Core concepts

- A **principal** is an authenticated human or service identity.
- An **actor** is the identity attributed to an operation by trusted infrastructure; it may represent a human, service, or agent.
- An **agent** is an orchestration configuration. Its capability list defines what it may propose, not what its process may execute directly.
- A **capability** names a narrow operation, its provider, effect kind, risk, idempotency, and least-privilege provider permissions.
- A **provider** is a declaration for an integration boundary. Existing credentials are selected symbolically by trusted configuration; a model-selected public DRN is separately typed, authorized, and matched to an owner-only use binding before the broker resolves it.
- A **proposal** is untrusted intent plus arguments.
- An **authorization** is a broker-owned state transition that binds a proposal to constraints and a decision receipt.
- **Evidence** supports later verification of a decision or execution result.
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
| `dekopon` | Human/operator CLI; reads typed resources, renders results, owns model-account lifecycle commands, and hosts the interactive console | **Current**, local catalog plus isolated model auth; `console` is the one command that contacts another process, as an unprivileged broker client holding a model credential and no authority |
| `dekopon-core` | Validated identifiers and dependency-light domain types | **Current** |
| `dekopon-protocol` | Versioned, transport-independent resource shapes | **Current** |
| `dekopon-config` | Config discovery, decoding, duplicate detection, and reference validation | **Current** |
| `dekopon-capability` | Capability metadata and proposal/authorization invocation states | **Current**, consumed by broker libraries and service |
| `dekopon-provider-sdk` | Rust guest trait, provider manifests/responses, and default or caller-generated WIT world export adapters | **Current**, experimental component contract |
| `dekopon-provider-http` | Rust guest facade for the buffered `dekopon:http@1.0.0` import; contains no transport or authority | **Current**, bindings only |
| `dekopon-provider-storage` | Feature-gated JSONL and durable-files guest bindings; contains no path, namespace, transaction, SQL, or authority API | **Current**, bindings only |
| `dekopon-provider-host` | Import-free Wasmtime component loading, limits, and read-only routing | **Current**, experimental and unprivileged |
| `dekopon-http-host` | Statically linked native buffered HTTP engine consuming exact grants beneath independent ceilings; contains no WIT or Wasmtime integration | **Current** library |
| `dekopon-storage-host` | Wasmtime-independent opaque namespace derivation, key/root hygiene, logical quotas, leases, invocation overlays, durable manifests/recovery, JSONL, durable files, and bounded GC | **Current** privileged library |
| `dekopon-broker-host` | Privileged async Wasmtime adapter consuming authorized invocations and exact optional storage grants, linking only versioned Dekopon HTTP/storage imports, and emitting bounded metadata | **Current** library used by the separate broker process |
| `dekopon-broker` | Trusted context binding, Cedar-decided authorization over owner-authored execution constraints, replay rejection/recovery, provider execution, digest evidence, and metadata-only hash-linked audit coordination | **Current** library with bounded in-memory and owner-only durable JSONL audit |
| `dekopon-policy` | Bounded, deterministic Cedar adapter: generated schema, strict startup validation, declared entity world, deny-on-error decisions, determining policy identifiers, policy-set digest | **Current** library consumed only by `dekopon-broker` and `dekopon-brokerd` |
| `dekopon-broker-protocol` | Lightweight strict versioned bounded frames and Unix client with identity/authority-free payloads and server peer-UID verification | **Current** shared broker/runner API with no privileged host or native-HTTP dependency |
| `dekopon-model` | Bounded chat-model contract, OpenAI-compatible transport, ChatGPT/Codex subscription auth and Responses client, plus a fixed-endpoint bounded OpenAI Images client | **Current**, consumed by both CLIs and the gateway |
| `dekopon-shell` | Sandboxed bash-flavored interpreter whose command words dispatch to capabilities through one abstract seam, with its own step, recursion, output, deadline, and capability-call bounds | **Current**; it links no Wasmtime, broker, HTTP, or filesystem code |
| `dekopon-process` | Unprivileged one-run/one-node Tokio lifecycle seam whose internal supervisor preserves a typed operation result or Tokio task failure and delivers it to a required abandonment observer if the outer caller is dropped while the runtime remains alive | **Current** library consumed by `dekopon-run shell`, whose runtime lives through normal command completion; scopes, ports, cancellation, deadlines, and stage scheduling remain future |
| `dekopon-agent` | The shared agent session layer: the bounded scripting prompt loop, optional bounded asset/configuration/image-generation meta tools, the script runtime spending a session-wide capability budget, and a broker-leg facade over the protocol client | **Current**, holding no authority; consumed by `dekopon-run`, `dekopond`, and `dekopon-tui` |
| `dekopon-telemetry` | OTLP exporter settings and subscriber wiring, with ingest credentials read only from the environment | **Current** library shared by the executables |
| `dekopon-tui` | The operator console: a terminal view over an attested broker session, observing the agent loop through decorators on the script and capability seams, with render-time redaction and terminal-control sanitisation | **Current**, holding a model credential and no authority; embedded only in `dekopon` |
| `dekopon-webui` | GET-only, unauthenticated operational HTML for broker-loaded providers, Wasmtime counters, credential-free OTLP settings, and bounded gateway-reported agent/token status | **Current** library embedded only in `dekopon-brokerd`; listener enablement is explicit |
| `dekopon-run` | One-shot direct invocation, a single model scripting tool, local/OTLP trace export, audit-safe lifecycle logs, and identity-free Unix broker proposal client without effect authority | **Current**, with deliberately separate direct and broker subcommands |
| `dekopond` | Chat-transport wakeups, including a signed text-only WhatsApp Cloud API webhook, attested routing, opt-in route-scoped image generation, text/image replies, authorization-fed Slack Agent thread ownership, optional no-reply decisions, best-effort native in-flight activity, cooperatively cancellable bounded agent sessions with no broker authority, credential-free self-inspection, and bounded per-sender conversation history | **Current** unprivileged daemon; a route is one independent session per message unless it opts into `mode: persistent`, generation/activity are explicit opt-ins after authorization, Slack continuation is installed only after authorization, WhatsApp TLS terminates outside the daemon and replay handling is process-local, and a dedicated gateway UID remains **committed direction** |
| `dekopon-brokerd` | Owner-only Unix peer authentication, Cedar authorization, legacy destination-bound credentials, public-DRN/private-map secret resolution after a separate `secret.use` decision, replay restoration, provider execution, evidence, durable audit, atomic local checkpoint verification, an explicitly enabled unauthenticated read-only web view, and a separate offline exact-reference OCI provider-manager mode | **Current** privileged process; secret descriptors are parsed without network at startup and one source snapshot resolves per authorized invocation; managed-provider startup remains network-free; workload-identity secret bootstraps, leased secrets, provenance verification, SemVer updates, pruning, and independent remote/signed audit anchoring remain future |
| Cedar policy adapter | Declarative authorization with strict startup validation and per-decision explanations | **Current**; it replaced the exact-match evaluator outright |
| Exact policy evaluator | Principal/actor/capability/provider rules with deny-by-default matching | **Removed.** Its authorization half is now Cedar; its execution half survives unchanged as owner-authored constraint sets, still validated against loaded manifests, host ceilings, and the credential store |
| Deployable privileged provider path | Authenticated broker ownership of policy, credentials, component-host execution, durable evidence, and authorized effects | **Current** local Unix foundation; stronger deployment transport remains direction |

The agent daemon must not gain effect authority merely because it coordinates a task. The broker must not perform model orchestration merely because it can execute a provider. Image generation is model inference rather than a provider effect: its explicitly named model credential stays inside the unprivileged gateway/model client, while the model can choose only a bounded prompt and never the endpoint, credential, filename, or authenticated chat destination.

### Provider storage and durable chat memory

**Status: current.** `dekopon-brokerd` may opt into a separate broker-only
storage root and namespace key. Exact `jsonl` or `durable-files` plus read-only/read-write authority
is bound to one authorization; HTTP and storage cannot coexist in one v1 capability. Raw scope and
logical names never select paths. Mutations remain provisional until a successful, bounded,
decoded provider result and then cross a synchronized transaction marker before becoming success.

The independently released optional `memory-chat` provider uses JSONL only. Hidden recording is reachable solely
through `RecordDeliveredTurnForChat` after complete gateway-attested transport acceptance. Recent
and literal case-insensitive search are on demand and never automatically seed a prompt. Both
continuity policies always include provider, agent, canonical sender, transport, channel, and
conversation: `stable` deliberately survives semantic authority changes; the default
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

The published `0.11.1` release retains the local catalog read path introduced in 0.1:

```text
parse dekopon CLI
  -> resolve one config source
  -> parse YAML/JSON once
  -> validate a typed catalog
  -> execute through LocalConfigReader
  -> render a typed result
```

Command handlers do not manipulate YAML. `LocalConfigReader` is the one reader; a read abstraction returns when a second implementation exists rather than in anticipation of one. Configuration is deterministic, rejects unknown authored fields, and validates duplicate names and cross-resource references.

Model-account lifecycle is a separate operator path that does not resolve or parse the catalog:

```text
parse dekopon auth chatgpt CLI
  -> contact OpenAI's fixed device-auth endpoint
  -> store, inspect, or remove Dekopon's isolated credential file
```

It also includes an explicitly experimental immediate provider path:

```text
parse dekopon-run CLI
  -> compile Wasm components and validate manifests
  -> reject duplicate routes and every non-read-only effect
  -> direct invocation or OpenAI-compatible/ChatGPT-subscription prompt/tool loop
  -> shell mode moves provider loading plus the synchronous interpreter to one joined blocking process node
  -> fresh bounded store per component call
  -> JSON result/timings and optional local Chrome plus remote OTLP telemetry
```

The immediate linker supplies no guest imports, so providers have no filesystem, network, clock, random, environment, or credential access. Prompt mode performs model HTTP requests, but model tool calls remain untrusted: a call selects the one scripting tool or ends the session, and the script it carries can reach only loaded capability IDs plus, with `--broker`, whatever a separate broker authorizes for this peer. Explicit `dekopon-run broker` commands load no components and use a fresh bounded Unix connection to submit identity-free proposals after validating the configured server UID. Separately, `dekopon-brokerd` evaluates Cedar authorization decisions against owner-authored execution constraints, resolves legacy destination-bound credentials or separately authorized public DRNs from owner-only storage, can execute policy-constrained provider HTTP, and produces durable audit evidence; the operator CLI does not invoke it.

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

The accepted state model is deliberately asymmetric:

```text
Proposed --broker denies----------------------> Denied result + evidence
    |
    +--broker authorizes--> Authorized --host executes--> Succeeded/Failed result
```

`ProposedInvocation` is publicly constructible because untrusted callers are allowed to express intent. `AuthorizedInvocation` is not publicly constructible from arbitrary fields. Broker authorization must validate its decision metadata and attach bounded `ExecutionConstraints`. Those constraints never come from the policy set: Cedar decides only whether the invocation is permitted, and the bounds come from the capability's owner-authored constraint set, so no policy edit can widen an execution bound.

The direct `dekopon-run` provider path does not cross this state boundary. Its immediate prompt tool calls are unprivileged requests accepted only for import-free components declaring `read-only`; they are not `AuthorizedInvocation` values. Adding provider I/O, credentials, local writes, or external writes to that in-process path would violate this design. The explicit broker-backed mode submits proposals over authenticated Unix transport, but only the separate broker may authorize them, resolve privileged imports, and execute effects.

The current local broker protocol and service define:

- peer-UID-authenticated principal and actor mapping with no payload identity fields;
- attested on-behalf-of proposals: a peer holding an owner-configured attestor grant may name a canonical external subject, and the broker alone maps that subject to a principal through owner-controlled configuration;
- `via` as policy context, so a policy that requires an attestor cannot authorize a direct peer and one that forbids it cannot authorize an attested proposal — configuring a gateway still cannot widen a grant that already existed;
- an explicit `agent.prompt` action, so permitting a principal to drive an agent's session is its own policy statement rather than a side effect of holding any capability;
- bounded invocation-ID replay protection restored from verified audit history;
- canonical proposal and decision identifiers;
- policy revision, the determining policy identifiers, and a policy-set digest in every decision record;
- timeout, output, exact network, and host-call constraints;
- exact idempotency metadata matching (automatic retries remain future work);
- evidence integrity and durable audit ordering;
- denial and partial-failure semantics; and
- bounded informational agent-inventory and model-usage reports accepted only from a mapped gateway attestor. These reports feed process-local UI state and never enter policy, authorization, provider routing, credentials, evidence, or durable audit.

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

Both processes now exist and are deployed separately over the local Unix transport; the WhatsApp public webhook terminates only in the unprivileged gateway and preserves the same separation. `dekopond` sends proposals on that authenticated connection and receives results; it does not receive or relay an `AuthorizedInvocation` as a wire grant. Its proposals are attested: it names the chat sender's canonical subject and the agent answering, and the broker alone maps that subject to a principal. Its contract is documented in [`dekopond.md`](dekopond.md). The automatic per-sender replay window lives inside that unprivileged process and never reaches the broker; optional durable turns travel only through the hidden storage-provider path, and neither caches authorization: a persistent conversation opens a fresh attested leg per message exactly as an independent session does. The broker may share a Wasmtime engine and compiled component cache, but each invocation gets a fresh store. Privileged providers run as bounded asynchronous invocations integrated with Tokio, with explicit limits on time, memory, output, network destinations, and host calls.

The immediate host exposes no imports and remains the only host used by `dekopon-run`. The JSONPlaceholder example proves separately named read-only/idempotent and external-write/non-idempotent provider operations against loopback mocks; its optional mock endpoint cannot widen exact broker authority. The privileged `dekopon-broker-host` uses Tokio and exposes only statically implemented Dekopon HTTP and storage interfaces, consumes constrained authorization plus an exact single-use storage grant where applicable, and creates a fresh bounded store per operation. `dekopon-broker` now binds a separately supplied authenticated context, asks `dekopon-policy` whether that context may act, binds an allow to the capability's owner-authored constraint set, rejects replays from verified durable history and the current process, constructs and consumes authorization, and records redacted decision/outcome metadata in bounded in-memory or durable verified JSONL chains. Durable reopen restores replay IDs and rejects mutation, insecure permissions, hard links, symlinks, concurrent writers, overlong records, and partial writes. The service synchronizes a separate atomic count/head checkpoint after each append, rejects valid-prefix rollback relative to that retained file, and recovers only the one-record audit-ahead crash window. The protocol/client library adds hard-bounded strict frames and verifies a configured server UID. `dekopon-brokerd` maps connected peer UID into trusted context, restores verified replay state before binding an owner-only socket, limits and drains connections, and exposes policy-authorized provider execution. Its separate `provider` operator mode now resolves fully qualified exact OCI tags or manifest digests into a strict generated lock and immutable local blobs, while ordinary startup performs no network access. A managed startup derives blob paths from that lock and compares expected length, component SHA-256, and provider ID with the exact buffer and bounded description the host consumes; legacy directly named `providers` remain compatible but do not acquire remote provenance. It also derives attested contexts: a peer's owner-configured attestor grant bounds which canonical subject namespaces it may speak for, owner-configured identity mappings alone turn a subject into a principal, and policy conditioned on `context.via` decides what that attested context may do. The peer supplies the subject and never the principal, and every refusal is an audited denial recorded against the peer. Isolating a gateway's authority from the rest of the owner's processes still requires giving it its own UID, which the owner-only socket does not yet support; that remains committed direction. It resolves legacy destination-bound credentials from a separate owner-only file, selecting per acting agent where a constraint set names one. Separately, an inert public DRN may arrive as typed proposal data: the broker requires ordinary capability policy, an exact `secret.use` Cedar grant, and an owner-only use binding before resolving one invocation-pinned source snapshot and handing only native Basic/Bearer material to the HTTP engine. It does not independently retain, sign, or remotely anchor checkpoint generations. Their accepted contract and remaining staged delivery are defined in [`broker-http.md`](broker-http.md). Cedar is now the authorization engine: the exact-match evaluator proved which inputs a decision actually needs, and those inputs — principal, action, provider, `via`, subject, agent, optional trusted chat transport/channel/conversation scope, and the trusted classification — are what the generated Cedar schema exposes. Arbitrary provider input remains absent from Cedar context. The sole typed caller-supplied exception is a public DRN in a separate `secret.use` request whose schema fixes the capability, provider and sink; possession still grants nothing and the owner binding remains the execution ceiling.

## The granularity of authority

**Status: current.** Five separate mechanisms decide what one message may cause, and each is the
unit of a different thing. They were built one at a time; this section states them as one model,
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
  call budgets — and the credential, which may now differ per acting agent. None of it is reachable
  from policy text.

What the composition buys: **two organizations, two tokens, two agents, one broker.** A
DekoponVille agent reachable from one channel presents a `dekopon-agents`-scoped token; a Nested Set
agent reachable from another presents a `scientist-hq`-scoped one. No capability is duplicated, no
provider is deployed twice, and no token is reachable from the wrong workspace — because the route
decides the agent, the agent decides the credential, and the policy decides whether that principal
may drive that agent at all. Revoking one is one policy statement or one mapping, and it does not
disturb the other.

What it does not buy: **general capability policy still cannot bind a provider-input path.** Cedar
sees the principal, action, provider, and trusted routing metadata; it does not inspect arbitrary
provider JSON, so no capability statement can say "this agent may comment on issues in
`dekopon-agents/*` only". The public-DRN path is deliberately narrower: a private secret-use binding
may constrain HTTP authority, method, canonical path and query presence at the native sink, but that
constrains where one secret is presented rather than interpreting repository/object identity in
provider input or request bodies. Upstream credential scope remains the boundary for those semantics.

## Operator interface

The CLI is the stable operator surface, analogous to `kubectl` where that improves discovery. Its pipeline remains parse → resolve → read → execute → render. Human-readable output may evolve; machine-readable resources, output formats, and documented exit behavior require compatibility consideration.

Future commands should continue the resource-oriented vocabulary (`get tasks`, `logs agent/reviewer`, `auth can-i`, `policy explain`, `apply`, `delete`) but must not be added as nonfunctional placeholders.

`console` is the deliberate exception to the read-resolve-read-render pipeline, and the boundary it
does not cross is worth stating. It runs an agent session in the operator's own process, which makes
it a gateway for one terminal — so it holds a model credential, exactly as `dekopond` does, and no
policy, provider credential, or authorization, exactly as `dekopond` does not. CI applies the same
dependency check to `dekopon` that it already applies to `dekopon-run` and `dekopond`. The reason it
runs the loop rather than driving one is that tool-call arguments and results exist only inside the
process running it: history keeps prompts and answers, spans keep argument counts, and audit records
keep digests.

See [`cli.md`](cli.md) for the current command contract.

## Accepted implementation decisions

| Decision | Rationale |
|---|---|
| One Cargo monorepo | Initial crates share versions, CI, issues, and security review and are changing together. |
| Cedar for authorization, owner-authored constraint sets for execution | A declarative policy language is the right tool for "who may do what" and the wrong place for a timeout, an allowed host, or a credential binding. Splitting them means a policy edit can broaden who may act and can never widen how far an action reaches. |
| Startup complains, invocation enforces | Whether configuration naming an absent capability refuses startup is an operator preference (`strict`), because that check is a tripwire rather than a control: the `unconstrained-capability` refusal at invocation is unconditional and is what actually denies. Tolerating lets a deployment ship policy that anticipates a provider it has not dropped in yet. A policy naming an absent capability is kept whole and the name registered as a schema-only phantom, never dropped — dropping a grant reading `action in [a, b]` because `b` is unloaded would silently revoke `a` as well, turning one missing provider into a mute agent. An undeclared *principal* is exempt and always fatal: principals come from owner-authored configuration, so naming one that does not exist is a typo rather than an anticipation. |
| Credentials bound per capability, overridable per agent | Which *operation* gets a legacy credential is a capability question — the confused deputy is "same component, different operation". Which *credential* an operation presents to a given caller is a separate question, and keying it on the agent reuses the partition routes already make. |
| Public DRNs are proposal names, never bearer grants | A model may choose among logical names only as untrusted typed intent. Use requires the capability decision, a separate Cedar `secret.use` decision, an owner binding narrower than capability HTTP authority, and an authorization-bound native sink. The provider sees neither DRN nor value, and existing implicit credentials remain the simpler default where the model has no reason to choose. |
| Edition 2024 with an explicit MSRV | Modern language surface while preserving a tested minimum toolchain. |
| Synchronous one-shot `0.1.0` paths | The catalog and immediate provider operations remain bounded commands separate from asynchronous broker machinery. |
| Strong identifier newtypes | Invalid and ambiguous names should fail at system boundaries, not deep in execution. |
| Strict decoding | Misspelled security-relevant fields must not be ignored. |
| `BTreeMap`-backed catalogs | Deterministic reads and output simplify review, testing, and automation. |
| Private authorization fields plus compile-fail tests | Prevent accidental in-process authority fabrication while acknowledging that process isolation remains necessary. |
| Import-free immediate providers | Provider traits, component ABI, routing, limits, prompt tools, timings, and traces can stabilize without prematurely granting host authority. |
| Broker-owned buffered HTTP | Privileged providers import a project-owned high-level HTTP contract, while only the separate broker implements networking, applies authorization constraints, and records evidence. |
| No native runtime plugins | Broker host services are statically linked; untrusted imports never trigger Rust library or package downloads. |
| Owner-only Unix broker IPC | Local payloads cannot claim identity; private socket peer UID maps exactly to trusted context, with the whole UID as one trust domain. |
| Desired provider set, generated lock, immutable store | Provider selection, exact OCI resolution, and installed bytes are different states. The lock is the atomic activation point; daemon startup is offline and the host checks the lock against the exact compiled buffer. Exact tags are never implicit SemVer ranges. |
| Gateway-held conversation history | Immediate replay remains a per-sender, compacted, window-bounded in-memory gateway feature. Optional durable memory is a separate broker-owned provider store: content is namespace-bound and model-hidden on write, omitted from audit/telemetry, and retrieved only on demand. Authorization stays uncached in both mechanisms. |
| No empty future crates | A package boundary must be justified by meaningful, tested behavior. |
| Explicit unauthenticated web listener | The web UI has no mutating route and receives no credential values, but provider schemas, artifact paths, agent names, receiver endpoints, and runtime capacity are deployment information. `dekopon-brokerd` opens no TCP listener unless the operator supplies `--http-bind`; the surrounding network is the access boundary. |

## How to evaluate a proposed change

Before implementation, answer:

1. Is the behavior current work, committed direction, or exploration?
2. Which process owns the data and which process owns the authority?
3. Can model or repository content influence a trusted identity or authorization field?
4. Does a read become an implicit write, or a broad capability replace a narrow one?
5. What evidence and audit linkage would the operation need?
6. Does the change preserve typed, transport-independent boundaries?
7. Is a new crate or dependency required by tested behavior today?
8. Which failure, serialization, CLI, and security tests prove the boundary?
9. Which documentation would become inaccurate if the change landed?

If authority ownership is unclear, stop and update the design before adding code.

## Related documents

- [`security-model.md`](security-model.md) — trust assumptions, threat boundaries, and limitations.
- [`architecture.md`](architecture.md) — current crate structure and planned deployment topology.
- [`development.md`](development.md) — source/test map, generated artifacts, validation, CI, and PR workflow.
- [`cli.md`](cli.md) — current catalog and model-auth operator contract, discovery, output, and exit codes.
- [`run.md`](run.md) — experimental immediate provider, prompt, limit, and tracing contract.
- [`inference.md`](inference.md) — model request types and wire shape, cache optimization and retention caveats, current conversation memory, and exploratory long-term memory.
- [`dekopond.md`](dekopond.md) — the unprivileged chat gateway's configuration, transports, session bounds, authorization flow, and committed conversation contract.
- [`broker-http.md`](broker-http.md) — committed broker-mediated HTTP contract and authority boundary.
- [`roadmap.md`](roadmap.md) — implementation sequence and deliberately deferred scope.
- [`README.md`](README.md) — documentation map and task-based reading guide.
