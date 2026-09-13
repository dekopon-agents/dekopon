# Dekopon documentation map

Start with [`design.md`](design.md). It carries the [constitution](design.md#constitution) every other document must agree with, separates current behavior from committed direction, and records the invariants every change must preserve.

## Reading paths

### Understand the project

Read in this order:

1. [`design.md`](design.md) — the constitution, product thesis, vocabulary, authority flow, boundaries, and accepted decisions.
2. [`development.md`](development.md) — source and test map, generated artifacts, separate workspaces, validation, CI, and PR workflow.
3. [`security-model.md`](security-model.md) — trusted and untrusted inputs, threat model, and present limitations.
4. [`architecture.md`](architecture.md) — how the design maps to crate boundaries and the two-process deployment.
5. [`cli.md`](cli.md) and [`dekopond.md`](dekopond.md) — the operator command surfaces and the long-running gateway.
   [`catalog.md`](catalog.md) is the field-by-field contract for the resources they all read, including which fields are load-bearing and which are reserved.
   [`chat-progress.md`](chat-progress.md) is the design of record for what a waiting person is shown while a session runs and for every way one is stopped.
   [`chatgpt-credential.md`](chatgpt-credential.md) follows the ChatGPT subscription credential from a local login to a pod.
6. [`inference.md`](inference.md) — exact model request types and wire shape, prompt-cache optimization and retention caveats, bounded chat history, durable on-demand chat turns, and the broader memory design space.
7. [`observability.md`](observability.md) — gateway and broker OTLP traces, the broker audit record and where it goes, what telemetry excludes, and the OpenObserve development example.
8. [`improvement.md`](improvement.md) — the operator-driven improvement loop: skills as progressive disclosure of operator-authored knowledge, opt-in `suggest_improvement` records, and what is absent.
9. [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries) — the host, policy, credential, and provider-lifecycle authority boundary, with status called out per slice.
10. [`secrets.md`](secrets.md) — public inert DRNs, separate `secret.use` authorization, the owner-only private map, executable source adapters, exact HTTP sinks, and rotation and reflection limits.
11. [`1password-eso.md`](1password-eso.md) — how a secret reaches a deployed daemon through 1Password and External Secrets, including the Kubernetes projection boundary the direct secret-map adapter handles separately.
12. [`operations.md`](operations.md) and [`upgrading.md`](upgrading.md) — running a deployment and moving it between releases. `operations.md` indexes the per-crate operational contracts, including where broker audit lives; `upgrading.md` records the breaking configuration migrations and the restart order.
13. [`roadmap.md`](roadmap.md) — intended sequence, not a claim that a component exists.

## Build a provider

**Status: Current; pre-production.** A Dekopon provider is executable WebAssembly Component code, not a native plugin, configuration file, or separate process. Its imports state structural requirements; only the selected host decides which interfaces exist.

The complete **[Build and run an import-free Wasm provider with Rust](https://dekopon-agents.github.io/guides/provider-sdk/)** walkthrough pins v0.7.0. Follow its exact versions as one tested set.

| Need | Start here |
|---|---|
| Import-free local computation | [`cli-probe`](../examples/providers/cli-probe/README.md), [`dekopon-provider-sdk`](../crates/dekopon-provider-sdk/README.md) |
| Broker-mediated buffered HTTP | [`dekopon-provider-jsonplaceholder`](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder), [`dekopon-provider-http`](../crates/dekopon-provider-http/README.md), and [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries) |
| Broker-mediated provider storage (`jsonl` or `durable-files`) | [`dekopon-provider-storage`](../crates/dekopon-provider-storage/README.md), [`dekopon-provider-sdk-testkit`](../crates/dekopon-provider-sdk-testkit/README.md), [`dekopon-storage-host`](../crates/dekopon-storage-host/README.md), and [`dekopon-brokerd` contract § Storage is a sibling privileged host interface](../crates/dekopon-brokerd/README.md#boundaries) |
| Provider checks and generated components | [`development.md`](development.md#provider-example-workspaces) |
| Resolve and lock deployed OCI provider bytes | [`dekopon-brokerd` § Managed provider sets](../crates/dekopon-brokerd/README.md#managed-provider-sets) |
| Trust boundaries and limitations | [`security-model.md`](security-model.md) |

A provider exports `describe`, `invoke`, and `run-command` — the `provider-cli` world, because the broker refuses a provider with capabilities and no command word — and imports nothing: no API for processes, host files, environment, networking, clock, randomness, or credentials. The broker links only explicitly supported Dekopon imports and authorizes every invocation. Wasmtime executes in the host process, and a production sandbox is a [non-goal](design.md#non-goals).

The broker additionally links only the project-owned `dekopon:http/client@1.0.0`, `dekopon:storage@0.1.0` (`jsonl` and `durable-files`), and `dekopon:clock/wall@1.0.0` interfaces, the storage package only under an exact storage grant and never together with HTTP in one capability, and the wall clock during an authorized invocation only; see [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries) and [`dekopon-provider-storage`](../crates/dekopon-provider-storage/README.md). Any broker invocation — including pure computation — requires operator-installed bytes, trusted identity mapping, an exact constraint set, Cedar policy, and path configuration. An HTTP provider also needs a composed WIT world and narrowly scoped authority. Provider code controls paths, queries, bodies, and endpoint semantics inside the host-enforced envelope, so use fixed request shapes and validate all input and responses.

If the design needs another import, private-network access, files, processes, streaming, durable guest state beyond the namespace-bound `dekopon:storage@0.1.0` grant, general provider-input path semantics, or authentication beyond destination-bound credentials and DRN-backed native Basic/Bearer sinks, treat it as a host or platform change rather than provider-only work. The legacy `credential`/`credentialByAgent` bindings [will be replaced by public DRNs](design.md#legacy-credential-bindings), preserving broker-owned resolution and injection.

Keep the host, SDK, HTTP and storage facades, provider WIT, HTTP WIT, storage WIT, and manifest API versions explicit. Matching host load tests, not version labels alone, prove compatibility.

### Change a specific area

| Work | Read | Why |
|---|---|---|
| Any behavior or architecture change | [`design.md`](design.md) | Establishes the constitution, invariants, ownership, terminology, and current-versus-direction status. |
| Capabilities, identity, policy, providers, credentials, evidence, effects, or retained conversation text | [`security-model.md`](security-model.md) | Defines trust boundaries and threats the change must address. |
| Crates, protocols, daemon/broker split, or dependencies | [`architecture.md`](architecture.md) | Defines implementation and deployment boundaries and explains absent machinery. |
| Source locations, tests, WIT, generated Wasm, CI, dependencies, packaging, or releases | [`development.md`](development.md) | Records the practical repository workflow and scope-specific checks. |
| The container image, its publication workflow, or a container deployment | [`container-image.md`](container-image.md) | Records that the image reuses the published release archives, what it contains, the numeric runtime UID, the baked provider paths, and the directory ownership both daemons demand. |
| Operator auth parsing, rendering, or exit codes | [`cli.md`](cli.md) | Records the operator contract. |
| Agent, capability, or provider resource fields, or what a catalog value actually decides | [`catalog.md`](catalog.md) | Records every `v1alpha1` field, its consumer, and which fields are reserved and read by nothing. |
| Running a deployment: startup refusals, where broker audit lives, draining, or where an operational contract lives | [`operations.md`](operations.md) | Indexes the per-crate operational contracts by operator question rather than by crate. |
| Moving a deployment between releases, or a breaking configuration change | [`upgrading.md`](upgrading.md) | Records the migrations the changelog only names, the lockstep rule, and the restart order. |
| Getting a ChatGPT subscription credential into a cluster | [`chatgpt-credential.md`](chatgpt-credential.md) | Records why an interactive login cannot run in a pod, and the seed-once lifecycle that follows from a rotating refresh token. |
| Model request types, ChatGPT wire JSON, prompt caching, provider retention, chat memory, or memory frameworks | [`inference.md`](inference.md) | Separates request and cache hints, bounded replay, and durable on-demand turns from undocumented subscription behavior and exploratory memory. |
| Prompt tools and sandboxed scripts | [`dekopon-agent`](../crates/dekopon-agent/README.md) and [`dekopon-shell`](../crates/dekopon-shell/README.md) | Shared orchestration, language, limits, and authority-free dispatch. |
| Chat transports, gateway configuration, routing, agent sessions, or conversation history | [`dekopond.md`](dekopond.md) | Records the daemon's configuration, transport semantics, session bounds, attested authorization flow, and the conversation contract. |
| What a running session shows, streamed answers, keep-alives, or stopping a run | [`chat-progress.md`](chat-progress.md) | Records the progress vocabulary, the per-session policy that owns the one editable message, what each transport can natively show, and the cancellation paths. |
| Daemon tracing, OTLP logs, OpenObserve, telemetry exclusions, model-token totals | [`observability.md`](observability.md) | Records signal semantics, exported accounting, configuration, what telemetry excludes, and end-to-end validation. |
| Skills, `read_skill`, improvement suggestions, or evaluating a changed instruction before it ships | [`improvement.md`](improvement.md) | Records the two operator-driven improvement mechanisms, how they compose into one loop, and the store, rewriter, grader, and cross-session memory that are absent. |
| Public DRNs, private source maps, secret-use policy, source adapters, path-bound Basic/Bearer sinks, or mounted secret and config files | [`secrets.md`](secrets.md) | Defines the complete secret-reference and resolution contract. |
| Deployment secrets, 1Password, External Secrets, or projecting a credential into a pod | [`1password-eso.md`](1password-eso.md) | Records the deployed secret-store configuration, the manual bootstrap a human owns, and how ESO materialization composes with Dekopon's secure-file and projection sources. |
| Broker-mediated provider HTTP, host imports, or broker client mode | [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries) | Records the accepted HTTP contract, process ownership, authorization, and delivery boundaries. |
| Prioritization or a proposed new crate | [`roadmap.md`](roadmap.md) | Shows sequencing and reserved package names; roadmap entries are not implementation claims. |

Implementation-level contracts live beside their code in `crates/*/README.md`, including the bounded native [`dekopon-http-host`](../crates/dekopon-http-host/README.md) engine, the namespace-bound [`dekopon-storage-host`](../crates/dekopon-storage-host/README.md) quota and direct-write engine, the privileged async [`dekopon-broker-host`](../crates/dekopon-broker-host/README.md) adapter, the bounded Cedar [`dekopon-policy`](../crates/dekopon-policy/README.md) adapter, [`dekopon-broker`](../crates/dekopon-broker/README.md) authorization, evidence, and audit coordination, identity-free [`dekopon-broker-protocol`](../crates/dekopon-broker-protocol/README.md) wire and client boundaries, the authenticated Unix [`dekopon-brokerd`](../crates/dekopon-brokerd/README.md) service, the [`dekopon-provider-sdk-testkit`](../crates/dekopon-provider-sdk-testkit/README.md) in-process fake broker, the shared bounded prompt loop in [`dekopon-agent`](../crates/dekopon-agent/README.md), and the unprivileged [`dekopond`](../crates/dekopond/README.md) gateway.

Provider fixtures and exact standalone-release fetches are documented under [`../examples/providers/`](../examples/providers/README.md); JSONPlaceholder, memory-chat, and the nineteen-capability GitHub provider ship from their own repositories. Guest HTTP, storage, and clock bindings are documented in [`../crates/dekopon-provider-http/README.md`](../crates/dekopon-provider-http/README.md), [`../crates/dekopon-provider-storage/README.md`](../crates/dekopon-provider-storage/README.md), and [`../crates/dekopon-provider-clock/README.md`](../crates/dekopon-provider-clock/README.md). [`../examples/conditional-write/`](../examples/conditional-write/README.md) is the end-to-end deployment those pieces assemble into: a Slack DM, a bounded read, a broker-injected credential, and an audited etag-pinned write with no delete authority. [`../examples/discord/`](../examples/discord/README.md) documents Discord bot installation, least-privilege permissions, routing, identity mapping, and bounded photo and file handling. [`../charts/dekopon/`](../charts/dekopon/README.md) is the Slack worked deployment as a Helm chart, and records why a Secret or ConfigMap volume cannot hold a file either daemon will accept.

Also read [`../CONTRIBUTING.md`](../CONTRIBUTING.md) before submitting a change and [`../SECURITY.md`](../SECURITY.md) before reporting a vulnerability.

## Documentation contract

Documentation is part of the reviewed behavior:

- Use **Current**, **Committed direction**, and **Exploration** as defined in `design.md` when status could be ambiguous.
- Present tense must not make an unimplemented component sound available.
- [`CHANGELOG.md`](../CHANGELOG.md) is the only history. A document describes what the tree does, not what a release changed.
- Update the relevant document, examples, and tests in the same change as behavior.
- Prefer one authoritative explanation with links over subtly different copies.
- The security invariant outranks convenience. A roadmap item does not override the design or security model.
- If code, tests, and documentation disagree about current behavior, treat the disagreement as a defect and make the resolution explicit.

Human maintainers own product decisions and authorization. Coding agents may propose and implement reviewable changes, but must not silently redefine authority boundaries, publish packages, weaken repository protections, or claim future work as complete.
