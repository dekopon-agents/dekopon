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
3. **Identity comes from a trusted envelope.** Model text and repository content cannot assert a trusted `Actor`, principal, or workload identity.
4. **Credentials belong to the broker boundary.** Agents, prompts, authored resources, normal logs, and evidence records must not contain provider credentials.
5. **Authorization is bound to execution.** A grant carries the proposal, policy decision, execution constraints, and receipt needed to prevent a provider host from executing a different or broader operation.
6. **Effects produce evidence and audit linkage.** Proposal, identity, decision, policy revision, execution outcome, and evidence must remain correlatable by invocation and trace identifiers.
7. **External-write authority requires process isolation.** Once writes exist, orchestration and broker authority run in separate processes and deployment units.
8. **Documentation must distinguish reality from direction.** Never describe a daemon, broker, policy engine, provider host, or effect as available before it is implemented and tested.

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
              | authenticated envelope + policy input
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

The broker owns the only authority transition in this flow. Rust visibility is useful defense in depth, but the actual control comes from authentication, process isolation, policy, credential separation, and enforcement at the provider host.

## Component boundaries

| Component | Authority and responsibility | Status |
|---|---|---|
| `dekopon` | Human/operator CLI; parses commands, reads typed resources, renders results | **Current**, local catalog only |
| `dekopon-core` | Validated identifiers and dependency-light domain types | **Current** |
| `dekopon-protocol` | Versioned, transport-independent resource shapes | **Current** |
| `dekopon-config` | Config discovery, decoding, duplicate detection, and reference validation | **Current** |
| `dekopon-capability` | Capability metadata and proposal/authorization invocation states | **Current**, no executing broker |
| `dekopond` | Model interaction, orchestration, context, memory, and unprivileged task coordination | **Committed direction** |
| `dekopon-brokerd` | Authentication, authorization, credentials, provider execution, evidence, and external effects | **Committed direction** |
| Policy evaluator | Declarative authorization decisions and explanations; Cedar is the intended engine after inputs stabilize | **Committed direction** |
| Provider host | Bounded capability execution; Wasmtime components are the intended isolation mechanism | **Committed direction** |

The agent daemon must not gain effect authority merely because it coordinates a task. The broker must not perform model orchestration merely because it can execute a provider.

## Current control path

Version `0.1.0` implements only a local read path:

```text
parse CLI
  -> resolve one config source
  -> parse YAML/JSON once
  -> validate a typed catalog
  -> execute through ResourceReader
  -> render a typed result
```

Command handlers do not manipulate YAML. `LocalConfigReader` implements `ResourceReader`; a future daemon client may implement the same read boundary. Configuration is deterministic, rejects unknown authored fields, and validates duplicate names and cross-resource references.

No current path performs model interaction, resolves credentials, authorizes an invocation, loads a plugin, or executes an external effect.

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

`ProposedInvocation` is publicly constructible because untrusted callers are allowed to express intent. `AuthorizedInvocation` is not publicly constructible from arbitrary fields. Broker authorization must validate its decision metadata and attach bounded `ExecutionConstraints`.

Before a real broker is introduced, its protocol must define at least:

- authenticated principal and workload identity;
- freshness and replay protection;
- canonical proposal and decision identifiers;
- policy revision and explainable decision output;
- expiration, timeout, output, network, and host-call constraints;
- idempotency and retry behavior;
- evidence integrity and durable audit ordering;
- denial and partial-failure semantics.

## Deployment and provider isolation

When external writes are implemented, the deployment boundary is:

```text
dekopond              unprivileged orchestration and model interaction
      |
      | authenticated proposal envelope
      v
dekopon-brokerd       policy, credentials, provider execution, effects
      |
      | constrained invocation
      v
Wasm provider          one narrow integration operation
```

The agent and broker will run as separate processes and separate pods. The broker may share a Wasmtime engine and compiled component cache, but each invocation gets a fresh store. Providers run as bounded asynchronous invocations integrated with Tokio, with explicit limits on time, memory, output, network destinations, and host calls.

Wasmtime, Tokio, and Cedar should be added only with meaningful, tested behavior. Their presence in the long-term design is not a reason to add them to the current dependency graph.

## Operator interface

The CLI is the stable operator surface, analogous to `kubectl` where that improves discovery. Its pipeline remains parse → resolve → read → execute → render. Human-readable output may evolve; machine-readable resources, output formats, and documented exit behavior require compatibility consideration.

Future commands should continue the resource-oriented vocabulary (`get tasks`, `logs agent/reviewer`, `auth can-i`, `policy explain`, `apply`, `delete`) but must not be added as nonfunctional placeholders.

See [`cli.md`](cli.md) for the current command contract.

## Accepted implementation decisions

| Decision | Rationale |
|---|---|
| One Cargo monorepo | Initial crates share versions, CI, issues, and security review and are changing together. |
| Edition 2024 with an explicit MSRV | Modern language surface while preserving a tested minimum toolchain. |
| Synchronous local `0.1.0` | Async and network machinery add cost without supporting a current operation. |
| Strong identifier newtypes | Invalid and ambiguous names should fail at system boundaries, not deep in execution. |
| Strict decoding | Misspelled security-relevant fields must not be ignored. |
| `BTreeMap`-backed catalogs | Deterministic reads and output simplify review, testing, and automation. |
| Private authorization fields plus compile-fail tests | Prevent accidental in-process authority fabrication while acknowledging that process isolation remains necessary. |
| Private testkit | Shared fixtures are useful internally but are not part of the public product API. |
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
- [`cli.md`](cli.md) — current operator contract, discovery, output, and exit codes.
- [`roadmap.md`](roadmap.md) — implementation sequence and deliberately deferred scope.
- [`README.md`](README.md) — documentation map and task-based reading guide.
