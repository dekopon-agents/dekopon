# Security model

This document expands the security constraints introduced in [`design.md`](design.md). Read it before changing capabilities, identity, policy, credentials, providers, evidence, audit behavior, or external effects.

## Foundational invariant

> A model may propose an invocation, but only the broker may turn it into an authorized invocation.

A capability name in an agent spec permits the agent to propose that operation. It does not grant process authority, credentials, or permission to call a provider directly.

## Security-relevant stages

1. **Model proposal** — untrusted model output names a capability and supplies untrusted arguments in a `ProposedInvocation`.
2. **Authorization decision** — the privileged broker authenticates the transport, derives the actor/workload from trusted mapping, evaluates policy and current context, then either denies the proposal or creates a constrained `AuthorizedInvocation` inside its execution boundary.
3. **External effect** — the broker consumes that authorization state while a narrow provider executes only the authorized capability using broker-held credentials and enforced constraints.
4. **Evidence** — policy decisions and provider execution produce digests or bounded records that support later verification.
5. **Audit record** — the broker links proposal, trusted identity, policy revision, authorization receipt, effect outcome, and evidence under an invocation and trace ID.

The local daemon-to-broker request carries only a proposal over an authenticated Unix connection, not identity claims or an `AuthorizedInvocation` for the broker to trust. The broker does not return transferable authorization to `dekopond`; a serialized authorization representation is inert audit/evidence data rather than a bearer grant.

Rust's private, non-cloneable `AuthorizedInvocation` fields and intentional absence of deserialization make accidental in-process fabrication or reuse harder. `AuthorizationGate::new` is public so a broker adapter can own the transition; constructing that handle does not authenticate a caller or evaluate policy. This is defense in depth only. The real authority boundary depends on separate processes, authenticated and replay-resistant requests, policy enforcement, authorization bound to execution, isolated credentials, provider sandboxing, and durable audit integrity.

## Trust boundaries

Trusted inputs are expected to include:

- principal and workload identity derived from authenticated transport plus owner-controlled mapping;
- broker configuration and policy installed by an authorized operator;
- broker-generated authorization receipts and audit sequencing;
- secrets obtained by the broker from an approved secret store.

Explicitly untrusted inputs include:

- model output, reasoning, tool names, and tool arguments;
- repository files, pull-request text, issues, comments, diffs, and fetched web content;
- provider responses until validated and bounded;
- identity claims embedded inside model text or repository content;
- local config supplied from an untrusted checkout.

A model or repository document cannot self-assert a trusted `Actor`. For the local broker, the connected peer UID and strict owner-controlled mapping own identity attribution; invocation payloads have no identity fields.

## Capability and effect rules

- Capabilities are narrow and name one effect class.
- External writes require an explicit capability; read access never implies write access.
- Provider permissions should be least privilege and independently enforced by provider credentials.
- Authorization constraints bind timeout, output size, exact HTTP destinations/methods, call counts, and byte ceilings.
- Retries account for declared idempotency and use provider-enforced idempotency keys where available.
- Credentials do not appear in agent prompts, authored catalogs, invocation evidence, or normal logs.
- A component import declares a required host interface; it never grants that interface or any transitive authority.
- Broker HTTP authorization binds exact destinations, methods, host-call counts, byte limits, and deadlines to one invocation.

The example reviewer has `github.pull-request.read` and the explicit external-write `github.pull-request.comment`. It does not have, and the example does not declare, `github.pull-request.approve`.

## Published 0.1.0 posture and immediate path

The `dekopon` catalog commands read operator-selected YAML or JSON, reject unknown fields, validate identifiers and references, and render declarations without network access. The isolated `dekopon auth` namespace is the only current exception: it manages model-account login against fixed authentication hosts. The CLI performs no model inference, provider credential resolution, authorization decisions, or external effects. Provider readiness in local config is descriptive data, not a verified connection.

The separate experimental `dekopon-run` path can contact an operator-selected OpenAI-compatible model endpoint or OpenAI's fixed ChatGPT/Codex subscription endpoints and execute read-only Wasm component functions. Its provider boundary is deliberately narrower than a real integration:

- provider manifests are strictly decoded and may declare only `read-only` capabilities;
- duplicate provider and capability IDs are rejected before model interaction;
- model-selected function names map only to the offered capability registry and arguments must be JSON objects;
- capability schemas constrain model-facing tool declarations but are not generally enforced by the host, so providers must validate operation-specific input;
- every description and invocation uses a fresh Wasmtime store with memory, fuel, wall-clock, input, and output limits; component calls are currently serialized;
- the component linker exposes no WASI or custom imports, so guests receive no filesystem, network, clock, random, environment, credential, or external-read authority;
- an optional model bearer token is read from a named environment variable and sent only to the selected compatible endpoint;
- `dekopon auth chatgpt` uses OpenAI's Codex device flow and stores refreshable credentials in a Dekopon-owned file (`0600` on Unix); the shared model client fixes authentication and inference hosts to `auth.openai.com` and `chatgpt.com` and never imports credentials from pi, OpenClaw, or Codex;
- model credentials and opaque encrypted reasoning replay data are not exposed to components, output, or telemetry fields;
- optional runner OTLP export sends generated performance spans and stable lifecycle events to an operator-selected endpoint, but omits prompts, model responses, model-authored script text and its output, provider input/output, credentials, broker socket paths, and raw errors;
- immediate success output is raw untrusted JSON, not broker evidence, an authorization receipt, or an `InvocationResult`;
- immediate tool calls are not `AuthorizedInvocation` values and cannot be used for local or external writes.

Chrome and OTLP trace/log fields omit prompts, model responses, component input/output, bearer tokens, and raw untrusted errors. The operator-selected telemetry endpoint still learns execution metadata such as service/model/provider/capability identifiers, timings, outcomes, and source locations. OTLP lifecycle logs are operational audit data, not authorized invocation evidence or a substitute for the broker's durable hash-linked log. Final text and machine-readable outputs remain untrusted data. Terminal table cells in the catalog CLI continue to remove control characters.

## Current privileged broker foundation

`dekopon-broker-host` is the privileged component library used only by the separately deployed `dekopon-brokerd` process. It links only `dekopon:http@1.0.0`, consumes one non-cloneable `AuthorizedInvocation` at its public invocation boundary, and runs each description or invocation in a fresh memory-, fuel-, input-, output-, and wall-clock-bounded asynchronous Wasmtime store. Provider description receives a linked but disabled HTTP context, and any attempted description-time call rejects the component. Policy denials remain terminal even if guest code catches the typed WIT error.

The statically linked native client enforces exact authority/port and method grants, request count and byte bounds, HTTPS by default, loopback-only explicitly authorized plaintext, DNS address validation and pinning, sensitive-header ownership, no redirects, no ambient proxy, no automatic decompression, and bounded response collection. Its evidence contains method, authorized authority, status, and byte counts—not paths, queries, headers, or bodies.

The JSONPlaceholder demonstration keeps post reads and creates in separate capability IDs with read-only/idempotent versus external-write/non-idempotent metadata. Its guest accepts only the exact production HTTPS origin or explicit literal loopback HTTP endpoints, but guest validation is not authority: broker policy independently pins the exact authority and GET/POST method. Provider tests inject responses and broker tests use ephemeral loopback servers; CI does not contact the public service. Transport error details, post inputs, outputs, paths, and bodies remain absent from audit.

`dekopon-broker` wraps that host with a transport-independent trusted context, exact deny-by-default rules, startup validation of provider metadata and host ceilings, a bounded replay ledger, single-use authorization construction, stable public outcomes, digest evidence, and metadata-only hash-linked in-memory or durable audit chains. Human/service actor principals must match transport principals; agent identities require an exact principal/actor rule. Inputs, provider outputs, URL paths/queries, headers, bodies, and credentials are absent from audit records. Authorization decisions are appended before execution; if terminal audit append fails, the error explicitly says provider work may already have completed. `BrokerError::unaudited_outcome` makes that distinction structural rather than a matter of error text, and `dekopon-brokerd` preserves it across the wire as the `outcome-unaudited` failure code so a client can tell "nothing executed, safe to resubmit under a fresh identifier" from "the effect may have happened, do not resubmit".

`FileAuditLog` uses an exclusively writer-locked owner-only single-link file opened without symlink following, verifies bounded JSONL records before append, synchronizes each append, rejects partial records, exposes exact chain-prefix comparison, and reconstructs replay IDs for restart. `dekopon-brokerd` compares it with a separate strict checkpoint containing record count and chain head. Every audit append precedes an atomic, synchronized checkpoint replacement; startup rejects a missing checkpoint for non-empty audit or a checkpoint that is not an exact retained prefix. An audit exactly one record ahead of its valid checkpoint is the intentionally recoverable crash window; a larger gap fails closed.

`dekopon-broker-protocol` defines strict versioned frames and an unprivileged Unix client. Invocation wire values omit principal, actor, policy, constraints, credentials, and authorization. Frame lengths have a hard ceiling before allocation, complete reads/writes time out, and the client checks owner-only socket metadata plus server peer UID.

`dekopon-brokerd` now performs server-side Unix socket acceptance and derives `AuthenticatedContext` from the connected peer UID plus exact trusted configuration. It requires a private non-symlink parent, creates an owner-only socket, refuses unsafe/live replacement, limits concurrent one-request connections, drains under a configured grace period, restores replay IDs before listening, and removes only its own socket inode. Its strict configuration and provider files must be single-link, server-owned, and not group/world writable; provider parents must also be protected, writable non-sticky ancestors are rejected, and socket/audit/checkpoint/lock parents must be owner-only. The checkpoint has a dedicated single-writer lock, rejects symlinks and hard links, and uses synchronized temporary-file replacement plus parent-directory synchronization. Because mode `0600` makes the socket one UID trust domain, every process under that UID can use its configured actor—use a dedicated UID when this matters.

The service performs no credential injection, process attestation, independent remote/signed checkpoint anchoring, or non-Unix network transport. Its checkpoint is durable and externally inspectable, but locally rolling back or deleting both the audit and checkpoint can defeat comparison unless checkpoint generations are independently retained. Its presence does not expand direct `dekopon-run`: immediate subcommands retain the separate empty linker and reject HTTP-importing fixtures. Explicit broker subcommands load no component, require a trusted server UID, validate socket metadata/peer credentials, and send proposals with no identity, policy, constraint, credential, or authorization field. Their normal dependency path stops at the lightweight protocol/provider-metadata crates; CI rejects privileged broker, native-HTTP, or broker-service dependencies in the runner binary.

## Threat-model limitations

The current project does not defend against a malicious process in the broker/client UID trust domain, a local user who can replace the binary, component, or owner-controlled config; a compromised host; dependency or compiler compromise; denial of service during component compilation or from adversarial model endpoints; coordinated rollback of both local audit and checkpoint state; or side channels. The Wasmtime limits reduce invocation risk but are not a production sandbox claim. The project has no unprivileged daemon integration, provider secret-store integration, per-process/client attestation, independent audit checkpoint retention/signing service, external evidence store, key management, revocation, tenancy isolation, or incident-response automation.

The committed first privileged-provider design is documented in [`broker-http.md`](broker-http.md). It preserves the separate broker boundary, keeps direct `dekopon-run` execution import-free, and treats HTTP imports as structural requirements rather than authority.

Future releases must threat-model confused-deputy attacks, prompt injection, credential exfiltration, provider escalation, SSRF and DNS rebinding, redirect escapes, TOCTOU between authorization and execution, duplicate external effects, malicious Wasm components, resource exhaustion, forged identity envelopes, audit tampering, and cross-tenant data leaks before claiming production readiness.
