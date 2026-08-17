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
4. **Provider credentials belong to the broker boundary.** Agents, prompts, authored resources, normal logs, and evidence records must not contain provider credentials. Model-endpoint credentials terminate inside the selected model client and never enter provider components.
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
- A **provider** is a declaration for an integration boundary. Its credential reference is symbolic; the broker resolves the actual secret.
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
| `dekopon` | Human/operator CLI; reads typed resources, renders results, and owns model-account lifecycle commands | **Current**, local catalog plus isolated model auth |
| `dekopon-core` | Validated identifiers and dependency-light domain types | **Current** |
| `dekopon-protocol` | Versioned, transport-independent resource shapes | **Current** |
| `dekopon-config` | Config discovery, decoding, duplicate detection, and reference validation | **Current** |
| `dekopon-capability` | Capability metadata and proposal/authorization invocation states | **Current**, consumed by broker libraries and service |
| `dekopon-provider-sdk` | Rust guest trait, provider manifests/responses, and default or caller-generated WIT world export adapters | **Current**, experimental component contract |
| `dekopon-provider-http` | Rust guest facade for the buffered `dekopon:http@1.0.0` import; contains no transport or authority | **Current**, bindings only |
| `dekopon-provider-host` | Import-free Wasmtime component loading, limits, and read-only routing | **Current**, experimental and unprivileged |
| `dekopon-http-host` | Statically linked native buffered HTTP engine consuming exact grants beneath independent ceilings; contains no WIT or Wasmtime integration | **Current** library |
| `dekopon-broker-host` | Privileged async Wasmtime adapter that consumes authorized invocations, links only `dekopon:http@1.0.0`, and emits bounded metadata | **Current** library used by the separate broker process |
| `dekopon-broker` | Trusted context binding, Cedar-decided authorization over owner-authored execution constraints, replay rejection/recovery, provider execution, digest evidence, and metadata-only hash-linked audit coordination | **Current** library with bounded in-memory and owner-only durable JSONL audit |
| `dekopon-policy` | Bounded, deterministic Cedar adapter: generated schema, strict startup validation, declared entity world, deny-on-error decisions, determining policy identifiers, policy-set digest | **Current** library consumed only by `dekopon-broker` and `dekopon-brokerd` |
| `dekopon-broker-protocol` | Lightweight strict versioned bounded frames and Unix client with identity/authority-free payloads and server peer-UID verification | **Current** shared broker/runner API with no privileged host or native-HTTP dependency |
| `dekopon-model` | Bounded model contract, OpenAI-compatible transport, and ChatGPT/Codex subscription auth and Responses client | **Current**, consumed by both CLIs |
| `dekopon-shell` | Sandboxed bash-flavored interpreter whose command words dispatch to capabilities through one abstract seam, with its own step, recursion, output, deadline, and capability-call bounds | **Current**; it links no Wasmtime, broker, HTTP, or filesystem code |
| `dekopon-agent` | The shared agent session layer: the bounded one-tool prompt loop, the script runtime spending a session-wide capability budget, and a broker-leg facade over the protocol client | **Current**, holding no authority; consumed by `dekopon-run` and `dekopond` |
| `dekopon-telemetry` | OTLP exporter settings and subscriber wiring, with ingest credentials read only from the environment | **Current** library shared by the executables |
| `dekopon-run` | One-shot direct invocation, a single model scripting tool, local/OTLP trace export, audit-safe lifecycle logs, and identity-free Unix broker proposal client without effect authority | **Current**, with deliberately separate direct and broker subcommands |
| `dekopond` | Chat-transport wakeups, attested routing, and bounded agent sessions with no broker authority | **Current** unprivileged daemon; context and memory remain direction |
| `dekopon-brokerd` | Owner-only Unix peer authentication, Cedar authorization, destination-bound credential resolution, replay restoration, provider execution, evidence, durable audit, and atomic local checkpoint verification | **Current** privileged process; independent remote/signed anchoring remains direction |
| Cedar policy adapter | Declarative authorization with strict startup validation and per-decision explanations | **Current**; it replaced the exact-match evaluator outright |
| Exact policy evaluator | Principal/actor/capability/provider rules with deny-by-default matching | **Removed.** Its authorization half is now Cedar; its execution half survives unchanged as owner-authored constraint sets, still validated against loaded manifests, host ceilings, and the credential store |
| Deployable privileged provider path | Authenticated broker ownership of policy, credentials, component-host execution, durable evidence, and authorized effects | **Current** local Unix foundation; stronger deployment transport remains direction |

The agent daemon must not gain effect authority merely because it coordinates a task. The broker must not perform model orchestration merely because it can execute a provider.

## Current control paths

The published `0.2.0` release retains the local catalog read path introduced in 0.1:

```text
parse dekopon CLI
  -> resolve one config source
  -> parse YAML/JSON once
  -> validate a typed catalog
  -> execute through ResourceReader
  -> render a typed result
```

Command handlers do not manipulate YAML. `LocalConfigReader` implements `ResourceReader`; a future daemon client may implement the same read boundary. Configuration is deterministic, rejects unknown authored fields, and validates duplicate names and cross-resource references.

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
  -> fresh bounded store per component call
  -> JSON result/timings and optional local Chrome plus remote OTLP telemetry
```

The immediate linker supplies no guest imports, so providers have no filesystem, network, clock, random, environment, or credential access. Prompt mode performs model HTTP requests, but model tool calls remain untrusted: a call selects the one scripting tool or ends the session, and the script it carries can reach only loaded capability IDs plus, with `--broker`, whatever a separate broker authorizes for this peer. Explicit `dekopon-run broker` commands load no components and use a fresh bounded Unix connection to submit identity-free proposals after validating the configured server UID. Separately, `dekopon-brokerd` evaluates Cedar authorization decisions against owner-authored execution constraints, resolves destination-bound provider credentials from owner-only storage, can execute policy-constrained provider HTTP, and produces durable audit evidence; the operator CLI does not invoke it.

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

Both processes now exist and are deployed separately over the local Unix transport; a non-local transport will preserve the same separation. `dekopond` sends proposals on that authenticated connection and receives results; it does not receive or relay an `AuthorizedInvocation` as a wire grant. Its proposals are attested: it names the chat sender's canonical subject and the agent answering, and the broker alone maps that subject to a principal. Its contract is documented in [`dekopond.md`](dekopond.md). The broker may share a Wasmtime engine and compiled component cache, but each invocation gets a fresh store. Privileged providers run as bounded asynchronous invocations integrated with Tokio, with explicit limits on time, memory, output, network destinations, and host calls.

The immediate host exposes no imports and remains the only host used by `dekopon-run`. The JSONPlaceholder example proves separately named read-only/idempotent and external-write/non-idempotent provider operations against loopback mocks; its optional mock endpoint cannot widen exact broker authority. The privileged `dekopon-broker-host` uses Tokio and exposes only the statically implemented, buffered `dekopon:http@1.0.0` interface, consumes constrained authorization state, and creates a fresh bounded store per operation. `dekopon-broker` now binds a separately supplied authenticated context, asks `dekopon-policy` whether that context may act, binds an allow to the capability's owner-authored constraint set, rejects replays from verified durable history and the current process, constructs and consumes authorization, and records redacted decision/outcome metadata in bounded in-memory or durable verified JSONL chains. Durable reopen restores replay IDs and rejects mutation, insecure permissions, hard links, symlinks, concurrent writers, overlong records, and partial writes. The service synchronizes a separate atomic count/head checkpoint after each append, rejects valid-prefix rollback relative to that retained file, and recovers only the one-record audit-ahead crash window. The protocol/client library adds hard-bounded strict frames and verifies a configured server UID. `dekopon-brokerd` maps connected peer UID into trusted context, restores verified replay state before binding an owner-only socket, limits and drains connections, and exposes policy-authorized provider execution. It also derives attested contexts: a peer's owner-configured attestor grant bounds which canonical subject namespaces it may speak for, owner-configured identity mappings alone turn a subject into a principal, and policy conditioned on `context.via` decides what that attested context may do. The peer supplies the subject and never the principal, and every refusal is an audited denial recorded against the peer. Isolating a gateway's authority from the rest of the owner's processes still requires giving it its own UID, which the owner-only socket does not yet support; that remains committed direction. It resolves destination-bound provider credentials from a separate owner-only file; it does not independently retain, sign, or remotely anchor checkpoint generations. Their accepted contract and remaining staged delivery are defined in [`broker-http.md`](broker-http.md). Cedar is now the authorization engine: the exact-match evaluator proved which inputs a decision actually needs, and those inputs — principal, action, provider, `via`, subject, agent, and the trusted classification — are what the generated Cedar schema exposes. Provider input is deliberately still absent from that context, because conditioning authorization on untrusted JSON needs a settled schema treatment first.

## Operator interface

The CLI is the stable operator surface, analogous to `kubectl` where that improves discovery. Its pipeline remains parse → resolve → read → execute → render. Human-readable output may evolve; machine-readable resources, output formats, and documented exit behavior require compatibility consideration.

Future commands should continue the resource-oriented vocabulary (`get tasks`, `logs agent/reviewer`, `auth can-i`, `policy explain`, `apply`, `delete`) but must not be added as nonfunctional placeholders.

See [`cli.md`](cli.md) for the current command contract.

## Accepted implementation decisions

| Decision | Rationale |
|---|---|
| One Cargo monorepo | Initial crates share versions, CI, issues, and security review and are changing together. |
| Cedar for authorization, owner-authored constraint sets for execution | A declarative policy language is the right tool for "who may do what" and the wrong place for a timeout, an allowed host, or a credential binding. Splitting them means a policy edit can broaden who may act and can never widen how far an action reaches. |
| Edition 2024 with an explicit MSRV | Modern language surface while preserving a tested minimum toolchain. |
| Synchronous one-shot `0.1.0` paths | The catalog and immediate provider operations remain bounded commands separate from asynchronous broker machinery. |
| Strong identifier newtypes | Invalid and ambiguous names should fail at system boundaries, not deep in execution. |
| Strict decoding | Misspelled security-relevant fields must not be ignored. |
| `BTreeMap`-backed catalogs | Deterministic reads and output simplify review, testing, and automation. |
| Private authorization fields plus compile-fail tests | Prevent accidental in-process authority fabrication while acknowledging that process isolation remains necessary. |
| Private testkit | Shared fixtures are useful internally but are not part of the public product API. |
| Import-free immediate providers | Provider traits, component ABI, routing, limits, prompt tools, timings, and traces can stabilize without prematurely granting host authority. |
| Broker-owned buffered HTTP | Privileged providers import a project-owned high-level HTTP contract, while only the separate broker implements networking, applies authorization constraints, and records evidence. |
| No native runtime plugins | Broker host services are statically linked; untrusted imports never trigger Rust library or package downloads. |
| Owner-only Unix broker IPC | Local payloads cannot claim identity; private socket peer UID maps exactly to trusted context, with the whole UID as one trust domain. |
| No empty future crates | A package boundary must be justified by meaningful, tested behavior. |

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
- [`dekopond.md`](dekopond.md) — the unprivileged chat gateway's configuration, transports, session bounds, and authorization flow.
- [`broker-http.md`](broker-http.md) — committed broker-mediated HTTP contract and authority boundary.
- [`roadmap.md`](roadmap.md) — implementation sequence and deliberately deferred scope.
- [`README.md`](README.md) — documentation map and task-based reading guide.
