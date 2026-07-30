# Security model

This document expands the security constraints introduced in [`design.md`](design.md). Read it before changing capabilities, identity, policy, credentials, providers, evidence, audit behavior, or external effects.

## Foundational invariant

> A model may propose an invocation, but only the broker may turn it into an authorized invocation.

A capability name in an agent spec permits the agent to propose that operation. It does not grant process authority, credentials, or permission to call a provider directly.

## Security-relevant stages

1. **Model proposal** — untrusted model output names a capability and supplies untrusted arguments in a `ProposedInvocation`.
2. **Authorization decision** — the privileged broker authenticates the message envelope, resolves the actor and workload, evaluates policy and current context, then either denies the proposal or creates a constrained `AuthorizedInvocation` inside its execution boundary.
3. **External effect** — the broker consumes that authorization state while a narrow provider executes only the authorized capability using broker-held credentials and enforced constraints.
4. **Evidence** — policy decisions and provider execution produce digests or bounded records that support later verification.
5. **Audit record** — the broker links proposal, trusted identity, policy revision, authorization receipt, effect outcome, and evidence under an invocation and trace ID.

The daemon-to-broker request carries a proposal in an authenticated envelope, not an `AuthorizedInvocation` for the broker to trust. The broker does not return transferable authorization to `dekopond`; a serialized authorization representation is inert audit/evidence data rather than a bearer grant.

Rust's private, non-cloneable `AuthorizedInvocation` fields and intentional absence of deserialization make accidental in-process fabrication or reuse harder. `AuthorizationGate::new` is public so a broker adapter can own the transition; constructing that handle does not authenticate a caller or evaluate policy. This is defense in depth only. The real authority boundary depends on separate processes, authenticated and replay-resistant requests, policy enforcement, authorization bound to execution, isolated credentials, provider sandboxing, and durable audit integrity.

## Trust boundaries

Trusted inputs are expected to include:

- message-envelope principal and workload identity established by authenticated infrastructure;
- broker configuration and policy installed by an authorized operator;
- broker-generated authorization receipts and audit sequencing;
- secrets obtained by the broker from an approved secret store.

Explicitly untrusted inputs include:

- model output, reasoning, tool names, and tool arguments;
- repository files, pull-request text, issues, comments, diffs, and fetched web content;
- provider responses until validated and bounded;
- identity claims embedded inside model text or repository content;
- local config supplied from an untrusted checkout.

A model or repository document cannot self-assert a trusted `Actor`. The trusted message envelope owns identity attribution; payload claims are data only.

## Capability and effect rules

- Capabilities are narrow and name one effect class.
- External writes require an explicit capability; read access never implies write access.
- Provider permissions should be least privilege and independently enforced by provider credentials.
- Authorization constraints bind timeout, output size, and future network scopes.
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
- model credentials and opaque encrypted reasoning replay data are not exposed to components, output, or trace fields;
- immediate success output is raw untrusted JSON, not broker evidence, an authorization receipt, or an `InvocationResult`;
- immediate tool calls are not `AuthorizedInvocation` values and cannot be used for local or external writes.

Chrome trace fields omit prompts, model responses, component input/output, and bearer tokens. Final text and machine-readable outputs remain untrusted data. Terminal table cells in the catalog CLI continue to remove control characters.

## Current privileged host foundation

`dekopon-broker-host` is a privileged library, not a deployed broker. It links only `dekopon:http@1.0.0`, consumes one non-cloneable `AuthorizedInvocation` at its public invocation boundary, and runs each description or invocation in a fresh memory-, fuel-, input-, output-, and wall-clock-bounded asynchronous Wasmtime store. Provider description receives a linked but disabled HTTP context, and any attempted description-time call rejects the component. Policy denials remain terminal even if guest code catches the typed WIT error.

The statically linked native client enforces exact authority/port and method grants, request count and byte bounds, HTTPS by default, loopback-only explicitly authorized plaintext, DNS address validation and pinning, sensitive-header ownership, no redirects, no ambient proxy, no automatic decompression, and bounded response collection. Its evidence contains method, authorized authority, status, and byte counts—not paths, queries, headers, or bodies.

`dekopon-broker` wraps that host with a transport-independent trusted context, exact deny-by-default rules, startup validation of provider metadata and host ceilings, a bounded process-lifetime replay ledger, single-use authorization construction, stable public outcomes, digest evidence, and a metadata-only hash-linked audit chain. Human/service actor principals must match transport principals; agent identities require an exact principal/actor rule. Inputs, provider outputs, URL paths/queries, headers, bodies, and credentials are absent from audit records. Authorization decisions are appended before execution; if terminal audit append fails, the error explicitly says provider work may already have completed.

These libraries perform no socket authentication, trusted workload discovery, credential injection, durable replay recovery, durable audit persistence, or network service. `AuthenticatedContext` construction is not authentication; a future transport must derive it from peer credentials. No workspace executable reaches the broker core today. Its presence does not expand `dekopon-run`: the immediate runner still uses its separate empty linker and rejects the HTTP-importing fixture.

## Threat-model limitations

The current project does not yet defend against a malicious local user who can replace the binary, component, or config; a compromised host; dependency or compiler compromise; denial of service during component compilation or from adversarial model endpoints; rollback of files or audit data; or side channels. The Wasmtime limits reduce invocation risk but are not a production sandbox claim. The project has no authenticated daemon protocol, restart-persistent replay defense, provider secret-store integration, deployable privileged broker path, durable audit storage/checkpointing, external evidence store, key management, revocation, tenancy isolation, or incident-response automation.

The committed first privileged-provider design is documented in [`broker-http.md`](broker-http.md). It preserves the separate broker boundary, keeps direct `dekopon-run` execution import-free, and treats HTTP imports as structural requirements rather than authority.

Future releases must threat-model confused-deputy attacks, prompt injection, credential exfiltration, provider escalation, SSRF and DNS rebinding, redirect escapes, TOCTOU between authorization and execution, duplicate external effects, malicious Wasm components, resource exhaustion, forged identity envelopes, audit tampering, and cross-tenant data leaks before claiming production readiness.
