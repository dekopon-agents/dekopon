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

Rust's private `AuthorizedInvocation` fields and intentional absence of deserialization make accidental in-process fabrication harder. This is defense in depth only. The real authority boundary depends on separate processes, authenticated and replay-resistant requests, policy enforcement, authorization bound to execution, isolated credentials, provider sandboxing, and durable audit integrity.

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

The example reviewer has `github.pull-request.read` and the explicit external-write `github.pull-request.comment`. It does not have, and the example does not declare, `github.pull-request.approve`.

## Current 0.1.0 posture

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

## Threat-model limitations

The current project does not yet defend against a malicious local user who can replace the binary, component, or config; a compromised host; dependency or compiler compromise; denial of service during component compilation or from adversarial model endpoints; rollback of files or audit data; or side channels. The immediate Wasmtime limits reduce invocation risk but are not a production sandbox claim. The project has no authenticated daemon protocol, replay defense, policy semantics, provider secret-store integration, privileged host-call interface, audit storage, evidence canonicalization, key management, revocation, tenancy isolation, or incident-response automation.

Future releases must threat-model confused-deputy attacks, prompt injection, credential exfiltration, provider escalation, TOCTOU between authorization and execution, duplicate external effects, malicious Wasm components, resource exhaustion, forged identity envelopes, audit tampering, and cross-tenant data leaks before claiming production readiness.
