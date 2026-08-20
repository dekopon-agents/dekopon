# Dekopon documentation map

Start with [`design.md`](design.md). It is the shared product and system-design entry point; it separates current behavior from accepted future constraints and records the invariants every change must preserve.

## Reading paths

### Understand the project

Read in this order:

1. [`design.md`](design.md) — product thesis, vocabulary, authority flow, boundaries, and accepted decisions.
2. [`development.md`](development.md) — source/test map, generated artifacts, separate workspaces, validation, CI, and PR workflow.
3. [`security-model.md`](security-model.md) — trusted and untrusted inputs, threat model, and present limitations.
4. [`architecture.md`](architecture.md) — how the design maps to current crates and planned processes.
5. [`cli.md`](cli.md), [`run.md`](run.md), and [`dekopond.md`](dekopond.md) — the current user-facing command surfaces and the long-running gateway.
   [`chatgpt-credential.md`](chatgpt-credential.md) follows the ChatGPT subscription credential across that boundary, from a local login to a pod.
6. [`inference.md`](inference.md) — exact model request types and wire shape, prompt-cache optimization and retention caveats, bounded Slack history, ecosystem memory patterns, and the unimplemented long-term-memory design space.
7. [`observability.md`](observability.md) — runner, broker, and gateway OTLP traces, audit-safe logs, data minimization, what conversation history and the prompt cache add, and the OpenObserve development example.
8. [`broker-http.md`](broker-http.md) — implemented host/policy foundation and committed authenticated broker-process design, with status called out per slice.
9. [`1password-eso.md`](1password-eso.md) — how a secret reaches a deployed daemon: the 1Password service account, the External Secrets store already running against it, and the file hygiene no Kubernetes volume satisfies.
10. [`roadmap.md`](roadmap.md) — intended sequence, not a promise that a component exists.

### Change a specific area

| Work | Read | Why |
|---|---|---|
| Any behavior or architecture change | [`design.md`](design.md) | Establishes invariants, ownership, terminology, and current-versus-future status. |
| Capabilities, identity, policy, providers, credentials, evidence, effects, or retained conversation text | [`security-model.md`](security-model.md) | Defines trust boundaries and threats the change must address. |
| Crates, protocols, daemon/broker split, or dependencies | [`architecture.md`](architecture.md) | Defines implementation and deployment boundaries and explains intentionally absent machinery. |
| Source locations, tests, WIT, generated Wasm, CI, dependencies, packaging, or releases | [`development.md`](development.md) | Records the practical repository workflow and scope-specific checks. |
| The container image, its publication workflow, or a container deployment | [`container-image.md`](container-image.md) | Records that the image reuses the published release archives, what it contains, the numeric runtime UID, the baked provider paths, and the directory ownership both daemons demand. |
| Operator auth, catalog commands, config discovery, rendering, or exit codes | [`cli.md`](cli.md) | Records the current operator contract. |
| Getting a ChatGPT subscription credential into a cluster | [`chatgpt-credential.md`](chatgpt-credential.md) | Records why an interactive login cannot run in a pod, and the seed-once lifecycle that follows from a rotating refresh token. |
| Model request types, ChatGPT wire JSON, prompt caching, provider retention, chat memory, or memory frameworks | [`inference.md`](inference.md) | Separates implemented request/cache hints and bounded history from undocumented subscription behavior and exploratory long-term memory. |
| Immediate provider loading, direct invocation, or prompt tools | [`run.md`](run.md) | Records the experimental runner contract and its deliberately restricted authority. |
| Chat transports, gateway configuration, routing, agent sessions, or conversation history | [`dekopond.md`](dekopond.md) | Records the daemon's configuration, transport semantics, session bounds, attested authorization flow, and the conversation contract. |
| Runner tracing, OTLP logs, OpenObserve, telemetry redaction, model-token totals, or the broker web UI | [`observability.md`](observability.md) | Records signal semantics, live-versus-exported accounting, configuration, data minimization, and end-to-end validation. |
| Deployment secrets, 1Password, External Secrets, or projecting a credential into a pod | [`1password-eso.md`](1password-eso.md) | Records the deployed secret-store configuration, the manual bootstrap a human owns, and why a mounted Secret is not yet a file a daemon will open. |
| Broker-mediated provider HTTP, host imports, or broker client mode | [`broker-http.md`](broker-http.md) | Records the accepted HTTP contract, process ownership, authorization, and delivery boundaries. |
| Prioritization or a proposed new crate | [`roadmap.md`](roadmap.md) | Shows sequencing and deferred package names; roadmap entries are not implementation claims. |

Implementation-level contracts live beside their code in `crates/*/README.md`, including the bounded native [`dekopon-http-host`](../crates/dekopon-http-host/README.md) engine, privileged async [`dekopon-broker-host`](../crates/dekopon-broker-host/README.md) adapter, bounded Cedar [`dekopon-policy`](../crates/dekopon-policy/README.md) adapter, [`dekopon-broker`](../crates/dekopon-broker/README.md) authorization/evidence/audit coordination, identity-free [`dekopon-broker-protocol`](../crates/dekopon-broker-protocol/README.md) wire/client boundaries, the authenticated Unix [`dekopon-brokerd`](../crates/dekopon-brokerd/README.md) service, its GET-only [`dekopon-webui`](../crates/dekopon-webui/README.md) operational view, the shared bounded prompt loop in [`dekopon-agent`](../crates/dekopon-agent/README.md), and the unprivileged [`dekopond`](../crates/dekopond/README.md) gateway. Provider examples and generated-component workflows are documented under [`../examples/providers/`](../examples/providers/README.md), including the broker-only [`JSONPlaceholder provider`](../examples/providers/jsonplaceholder/README.md) and the nineteen-capability [`gh provider`](../examples/providers/gh/README.md); guest HTTP bindings are documented in [`../crates/dekopon-provider-http/README.md`](../crates/dekopon-provider-http/README.md). [`../examples/pr-summarizer-linter/`](../examples/pr-summarizer-linter/README.md) is the end-to-end deployment those pieces assemble into: a Slack DM, bounded PR inspection, a broker-injected credential, and an audited review comment with no approval or merge authority. [`../charts/dekopon/`](../charts/dekopon/README.md) is that deployment as a Helm chart, and records why a Secret or ConfigMap volume cannot hold a file either daemon will accept.

Also read [`../CONTRIBUTING.md`](../CONTRIBUTING.md) before submitting a change and [`../SECURITY.md`](../SECURITY.md) before reporting a vulnerability.

## Documentation contract

Documentation is part of the reviewed behavior:

- Use **Current**, **Committed direction**, and **Exploration** as defined in `design.md` when status could be ambiguous.
- Present tense must not make an unimplemented component sound available.
- Update the relevant document, examples, and tests in the same change as behavior.
- Prefer one authoritative explanation with links over subtly different copies.
- The security invariant outranks convenience. A roadmap item does not override the design or security model.
- If code, tests, and documentation disagree about current behavior, treat the disagreement as a defect and make the resolution explicit.

Human maintainers own product decisions and authorization. Coding agents may propose and implement reviewable changes, but must not silently redefine authority boundaries, publish packages, weaken repository protections, or claim future work as complete.
