# Security model

## Foundational invariant

> A model may propose an invocation, but only the broker may turn it into an authorized invocation.

A capability name in an agent spec permits the agent to propose that operation. It does not grant process authority, credentials, or permission to call a provider directly.

## Security-relevant stages

1. **Model proposal** — untrusted model output names a capability and supplies untrusted arguments in a `ProposedInvocation`.
2. **Authorization decision** — the privileged broker authenticates the message envelope, resolves the actor and workload, evaluates policy and current context, then either denies the proposal or creates a constrained `AuthorizedInvocation`.
3. **External effect** — a narrow provider executes only the authorized capability using broker-held credentials and enforced constraints.
4. **Evidence** — policy decisions and provider execution produce digests or bounded records that support later verification.
5. **Audit record** — the broker links proposal, trusted identity, policy revision, authorization receipt, effect outcome, and evidence under an invocation and trace ID.

Rust's private `AuthorizedInvocation` fields make accidental in-process fabrication harder. This is defense in depth only. The real authority boundary depends on separate processes, authenticated requests, policy enforcement, isolated credentials, provider sandboxing, and durable audit integrity.

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

The local CLI reads operator-selected YAML or JSON, rejects unknown fields, validates identifiers and references, and renders declarations. It performs no network requests, model calls, credential resolution, authorization decisions, or external effects. Provider readiness in local config is descriptive data, not a verified connection.

Terminal table cells remove control characters. Machine-readable output preserves authored strings and must still be handled as untrusted data by downstream consumers.

## Threat-model limitations

The current project does not yet defend against a malicious local user who can replace the binary or config, a compromised host, dependency or compiler compromise, denial of service from very large input files, rollback of files or audit data, or side channels. It has no authenticated daemon protocol, replay defense, policy semantics, secret-store integration, Wasm sandbox, audit storage, evidence canonicalization, key management, revocation, tenancy isolation, or incident-response automation.

Future releases must threat-model confused-deputy attacks, prompt injection, credential exfiltration, provider escalation, TOCTOU between authorization and execution, duplicate external effects, malicious Wasm components, resource exhaustion, forged identity envelopes, audit tampering, and cross-tenant data leaks before claiming production readiness.
