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
   [`catalog.md`](catalog.md) is the field-by-field contract for the resources they all read, including which fields are load-bearing and which are reserved.
   [`chatgpt-credential.md`](chatgpt-credential.md) follows the ChatGPT subscription credential across that boundary, from a local login to a pod.
6. [`inference.md`](inference.md) — exact model request types and wire shape, prompt-cache optimization and retention caveats, bounded Slack history, optional durable on-demand chat turns, ecosystem memory patterns, and the broader-memory design space.
7. [`observability.md`](observability.md) — runner, broker, and gateway OTLP traces, audit-safe logs, data minimization, what conversation history and the prompt cache add, and the OpenObserve development example.
8. [`broker-http.md`](broker-http.md) — implemented host/policy foundation and committed authenticated broker-process design, with status called out per slice.
9. [`secrets.md`](secrets.md) — public inert DRNs, separate `secret.use` authorization, the owner-only private map, executable source adapters, exact HTTP sinks, and rotation/reflection limits.
10. [`1password-eso.md`](1password-eso.md) — how a secret reaches a deployed daemon through 1Password and External Secrets, including the Kubernetes projection boundary the direct secret-map adapter now handles separately.
11. [`operations.md`](operations.md) and [`upgrading.md`](upgrading.md) — running a deployment and moving it between releases. `operations.md` is the index into the per-crate operational contracts, including audit checkpoint recovery; `upgrading.md` records the breaking configuration migrations and the restart order.
12. [`roadmap.md`](roadmap.md) — intended sequence, not a promise that a component exists.

## Build a provider

**Status: Current; pre-production.** A Dekopon provider is executable WebAssembly Component code, not a native plugin, configuration file, or separate process. Its imports state structural requirements; only the selected host decides which interfaces exist.

The complete **[Build and run an import-free Wasm provider with Rust](https://dekopon-agents.github.io/guides/provider-sdk/)** walkthrough deliberately pins v0.7.0. Every release since has left the provider contract and host path unchanged, but follow the guide's exact versions as one tested set.

| Need | Start here |
|---|---|
| Import-free local computation | [`dekopon-provider-echo`](https://github.com/dekopon-agents/dekopon-provider-echo), [`dekopon-provider-sdk`](../crates/dekopon-provider-sdk/README.md), and [`run.md`](run.md#rust-provider-interface) |
| Broker-mediated buffered HTTP | [`dekopon-provider-jsonplaceholder`](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder), [`dekopon-provider-http`](../crates/dekopon-provider-http/README.md), and [`broker-http.md`](broker-http.md) |
| Provider checks and generated components | [`development.md`](development.md#provider-example-workspaces) |
| Resolve and lock deployed OCI provider bytes | [`dekopon-brokerd` § Managed provider sets](../crates/dekopon-brokerd/README.md#managed-provider-sets) |
| Trust boundaries and limitations | [`security-model.md`](security-model.md) |

The base world exports `describe` and `invoke` and imports nothing. Direct `dekopon-run` accepts only declared read-only capabilities and links no provider host services. Under those interfaces, a component has no API for processes, host files, environment, networking, clock, randomness, or credentials. Wasmtime still executes in the host process; its limits are not a production sandbox claim.

The broker additionally links only `dekopon:http/client@1.0.0`. Any broker invocation—including pure computation—requires operator-installed bytes, trusted identity mapping, an exact constraint set, Cedar policy, and audit/path configuration. Existing HTTP providers also need a composed WIT world and narrowly scoped authority. Provider code controls paths, queries, bodies, and endpoint semantics inside the host-enforced envelope, so use fixed request shapes and validate all input and responses.

If the design needs another import, private-network access, files, processes, streaming, durable guest state, general provider-input path semantics, or authentication beyond legacy destination-bound credentials and the current DRN-backed native Basic/Bearer sinks, treat it as a host/platform change rather than provider-only work.

Keep the host, SDK, HTTP facade, provider WIT, HTTP WIT, and manifest API versions explicit. Matching host load tests—not version labels alone—prove compatibility.

### Change a specific area

| Work | Read | Why |
|---|---|---|
| Any behavior or architecture change | [`design.md`](design.md) | Establishes invariants, ownership, terminology, and current-versus-future status. |
| Capabilities, identity, policy, providers, credentials, evidence, effects, or retained conversation text | [`security-model.md`](security-model.md) | Defines trust boundaries and threats the change must address. |
| Crates, protocols, daemon/broker split, or dependencies | [`architecture.md`](architecture.md) | Defines implementation and deployment boundaries and explains intentionally absent machinery. |
| Source locations, tests, WIT, generated Wasm, CI, dependencies, packaging, or releases | [`development.md`](development.md) | Records the practical repository workflow and scope-specific checks. |
| The container image, its publication workflow, or a container deployment | [`container-image.md`](container-image.md) | Records that the image reuses the published release archives, what it contains, the numeric runtime UID, the baked provider paths, and the directory ownership both daemons demand. |
| Operator auth, catalog commands, config discovery, rendering, or exit codes | [`cli.md`](cli.md) | Records the current operator contract. |
| Agent, capability, or provider resource fields, or what a catalog value actually decides | [`catalog.md`](catalog.md) | Records every `v1alpha1` field, its consumer, and which fields are reserved and read by nothing. |
| Running a deployment: startup refusals, audit recovery, draining, or where an operational contract lives | [`operations.md`](operations.md) | Indexes the per-crate operational contracts by operator question rather than by crate. |
| Moving a deployment between releases, or a breaking configuration change | [`upgrading.md`](upgrading.md) | Records the migrations the changelog only names, the lockstep rule, and the restart order. |
| Getting a ChatGPT subscription credential into a cluster | [`chatgpt-credential.md`](chatgpt-credential.md) | Records why an interactive login cannot run in a pod, and the seed-once lifecycle that follows from a rotating refresh token. |
| Model request types, ChatGPT wire JSON, prompt caching, provider retention, chat memory, or memory frameworks | [`inference.md`](inference.md) | Separates request/cache hints, bounded replay, and optional durable on-demand turns from undocumented subscription behavior and broader exploratory memory. |
| Immediate provider loading, direct invocation, or prompt tools | [`run.md`](run.md) | Records the experimental runner contract and its deliberately restricted authority. |
| Chat transports, gateway configuration, routing, agent sessions, or conversation history | [`dekopond.md`](dekopond.md) | Records the daemon's configuration, transport semantics, session bounds, attested authorization flow, and the conversation contract. |
| Runner tracing, OTLP logs, OpenObserve, telemetry redaction, model-token totals, or the broker web UI | [`observability.md`](observability.md) | Records signal semantics, live-versus-exported accounting, configuration, data minimization, and end-to-end validation. |
| Public DRNs, private source maps, secret-use policy, source adapters, path-bound Basic/Bearer sinks, or mounted secret/config files | [`secrets.md`](secrets.md) | Defines the complete current secret-reference and resolution contract. |
| Deployment secrets, 1Password, External Secrets, or projecting a credential into a pod | [`1password-eso.md`](1password-eso.md) | Records the deployed secret-store configuration, the manual bootstrap a human owns, and how ESO materialization composes with Dekopon's secure-file and projection sources. |
| Broker-mediated provider HTTP, host imports, or broker client mode | [`broker-http.md`](broker-http.md) | Records the accepted HTTP contract, process ownership, authorization, and delivery boundaries. |
| Prioritization or a proposed new crate | [`roadmap.md`](roadmap.md) | Shows sequencing and deferred package names; roadmap entries are not implementation claims. |

Implementation-level contracts live beside their code in `crates/*/README.md`, including the bounded native [`dekopon-http-host`](../crates/dekopon-http-host/README.md) engine, privileged async [`dekopon-broker-host`](../crates/dekopon-broker-host/README.md) adapter, bounded Cedar [`dekopon-policy`](../crates/dekopon-policy/README.md) adapter, [`dekopon-broker`](../crates/dekopon-broker/README.md) authorization/evidence/audit coordination, identity-free [`dekopon-broker-protocol`](../crates/dekopon-broker-protocol/README.md) wire/client boundaries, the authenticated Unix [`dekopon-brokerd`](../crates/dekopon-brokerd/README.md) service, its GET-only [`dekopon-webui`](../crates/dekopon-webui/README.md) operational view, the shared bounded prompt loop in [`dekopon-agent`](../crates/dekopon-agent/README.md), and the unprivileged [`dekopond`](../crates/dekopond/README.md) gateway. Provider fixtures and exact standalone-release fetches are documented under [`../examples/providers/`](../examples/providers/README.md); Echo, JSONPlaceholder, memory-chat, and the nineteen-capability GitHub provider ship from their standalone repositories rather than from core. Guest HTTP bindings are documented in [`../crates/dekopon-provider-http/README.md`](../crates/dekopon-provider-http/README.md). [`../examples/conditional-write/`](../examples/conditional-write/README.md) is the end-to-end deployment those pieces assemble into: a Slack DM, a bounded read, a broker-injected credential, and an audited etag-pinned write with no delete authority. [`../examples/discord/`](../examples/discord/README.md) documents Discord bot installation, least-privilege permissions, routing, identity mapping, and bounded photo/file handling. [`../charts/dekopon/`](../charts/dekopon/README.md) is the Slack worked deployment as a Helm chart, and records why a Secret or ConfigMap volume cannot hold a file either daemon will accept.

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
