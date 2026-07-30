# Dekopon documentation map

Start with [`design.md`](design.md). It is the shared product and system-design entry point; it separates current behavior from accepted future constraints and records the invariants every change must preserve.

## Reading paths

### Understand the project

Read in this order:

1. [`design.md`](design.md) — product thesis, vocabulary, authority flow, boundaries, and accepted decisions.
2. [`development.md`](development.md) — source/test map, generated artifacts, separate workspaces, validation, CI, and PR workflow.
3. [`security-model.md`](security-model.md) — trusted and untrusted inputs, threat model, and present limitations.
4. [`architecture.md`](architecture.md) — how the design maps to current crates and planned processes.
5. [`cli.md`](cli.md) and [`run.md`](run.md) — the two current user-facing command surfaces.
6. [`broker-http.md`](broker-http.md) — implemented host/policy foundation and committed authenticated broker-process design, with status called out per slice.
7. [`roadmap.md`](roadmap.md) — intended sequence, not a promise that a component exists.

### Change a specific area

| Work | Read | Why |
|---|---|---|
| Any behavior or architecture change | [`design.md`](design.md) | Establishes invariants, ownership, terminology, and current-versus-future status. |
| Capabilities, identity, policy, providers, credentials, evidence, or effects | [`security-model.md`](security-model.md) | Defines trust boundaries and threats the change must address. |
| Crates, protocols, daemon/broker split, or dependencies | [`architecture.md`](architecture.md) | Defines implementation and deployment boundaries and explains intentionally absent machinery. |
| Source locations, tests, WIT, generated Wasm, CI, dependencies, packaging, or releases | [`development.md`](development.md) | Records the practical repository workflow and scope-specific checks. |
| Operator auth, catalog commands, config discovery, rendering, or exit codes | [`cli.md`](cli.md) | Records the current operator contract. |
| Immediate provider loading, direct invocation, prompt tools, or tracing | [`run.md`](run.md) | Records the experimental runner contract and its deliberately restricted authority. |
| Broker-mediated provider HTTP, host imports, or broker client mode | [`broker-http.md`](broker-http.md) | Records the accepted HTTP contract, process ownership, authorization, and delivery boundaries. |
| Prioritization or a proposed new crate | [`roadmap.md`](roadmap.md) | Shows sequencing and deferred package names; roadmap entries are not implementation claims. |

Implementation-level contracts live beside their code in `crates/*/README.md`, including the bounded native [`dekopon-http-host`](../crates/dekopon-http-host/README.md) engine, privileged async [`dekopon-broker-host`](../crates/dekopon-broker-host/README.md) adapter, and exact-policy [`dekopon-broker`](../crates/dekopon-broker/README.md) coordination boundaries. The provider example and generated-component workflow are documented in [`../examples/providers/echo/README.md`](../examples/providers/echo/README.md); guest HTTP bindings are documented in [`../crates/dekopon-provider-http/README.md`](../crates/dekopon-provider-http/README.md).

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
